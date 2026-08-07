# Changelog

All notable changes to Cyclops v2. Format follows Keep a Changelog;
versions are unreleased until admin cuts a tag.

## [Unreleased]

### Added (v4)

- Ten more themes, seventeen in all. Six bright originals drawn on paper
  rather than inverted from a dark theme (`sorbet`, `meadow`,
  `periwinkle`, `blossom`, `seafoam`, `buttercream`) and four dark
  originals (`midnight`, `ember`, `forest`, `obsidian`). Each carries all
  42 tokens with explicit 256-color fallbacks and maps the ANSI-16
  palette at ink strength, so an agent CLI printing in red, green or
  yellow stays legible on that theme's pane ground.
- `cyclops start --agents <id>[,<id>...]` names the CLIs for a preset's
  labeled panes and starts them as the panes are built. Ids are matched
  against installed manifests, and an unknown id or a count that does not
  fit the preset is refused before a single pane exists. The commands
  persist into the workspace file, and replaying them later still takes
  an explicit `--launch`: naming CLIs at the keyboard is a decision about
  this run, not a standing one. Launched CLIs have no hooks wired, so
  their receipts stay screen-verified until `cyclops hooks install`.
- Manifests upgrade the same way themes do. A manifest file still
  byte-identical to a version Cyclops shipped is a seed nobody edited and
  is replaced by the newer shipped body; anything else is the operator's
  measurement and is never touched. Without this, `--agents` was dead on
  every home seeded before the feature existed, because the shipped
  `launch` key could not reach it.
- Pane swap moves to a dedicated grip. Every pane paints a one-cell `⠿`
  handle in its bottom-right border corner, and dragging that cell onto
  another pane swaps the two with the drop resolution and focus-follows
  behavior frame-drag had (F43). The rest of the frame is no longer a
  drag handle: a left-click focuses the pane, so the resize seam between
  stacked panes is never shadowed by a swap pickup and a labeled bottom
  pane's seam resizes again. Keyboard swap chords are untouched, and the
  keybinds sheet teaches the grip from the painted glyph itself.
- The sidebar collapses and carries tabs. `Ctrl+B` `b` (config key
  `toggle_sidebar`) hands the sidebar's columns to the pane canvas; the
  state persists, so a workspace quit collapsed reopens collapsed.
  Collapsing takes the `☰ menu` button with it, so the chord is
  deliberately the only route in or out: no menu item can hide the panel
  that carries the menu.
- The sidebar's header carries `Sessions` and `Stream` chips. Click
  either to swap what the panel shows; the choice persists under
  `[workspace] sidebar_tab`. The chips take the row the plain
  "Workspaces" title used to hold, so the session tree keeps every row
  it had.
- `cyclops update`. One verb to rebuild from the latest source and
  replace the installed binaries in place. It names the build you are
  running, asks the source whether anything is newer first
  (`git ls-remote`, one round trip; already current exits 0 and stops;
  a `.dirty` or `unknown` build says why there is no check and
  proceeds), clones `CYCLOPS_REPO` at `CYCLOPS_REF` and streams that
  clone's installer, then reports old build to new from the new
  binary's own `--version` and closes with the three restart steps.
  Nothing is restarted for you; config, themes, manifests and the
  record are untouched. Wiring agent hooks stays
  `cyclops hooks install`, a different job.
- `cyclops list` scopes to the caller's session. Inside tmux, when
  exactly one watched session holds the caller's pane, the roster is
  that session's alone, with a dim line under the header naming the
  elided sessions and `cyclops list --all` as the way out. Outside
  tmux, on no match, and with `--all`, the output is byte for byte what
  it was. `--json` scopes identically and carries the elided names as
  an additive `also_watching` field (parity, not a second shape).
- The tab bar earns its row. With a single tab the strip hides and the
  pane canvas reclaims the row; it returns the moment a second tab
  exists. The painted chrome and the tmux-declared client size derive
  the bar's visibility from the same model snapshot, so the grid never
  drifts a row from what tmux was told. `prefix+c` and the menu's New
  tab still create tabs while the bar is hidden.

### Changed (v4)

- The event stream moved out of its own right-hand slide-out and into
  the sidebar's Stream tab. It inherits the sidebar's drag-to-resize and
  persisted width instead of a hardcoded 40 columns, and its rows still
  come from the one `event_stream_rows` path `cyclops watch` parity is
  proven against. `Ctrl+B` `e` and the app menu's Event stream item keep
  their names: show the stream, press again for the session list back.
  The trade is real: the session list and the event stream now share one
  panel and cannot be read at the same time.

### Fixed (v4)

- The workspace's daemon subscription outlives cyclopsd. A boot-order
  race or a daemon restart used to kill the state-event thread
  silently, so the idle/working words froze until a structural
  reconcile (the alt-tab-fixes-it symptom). The subscription now
  reconnects on a bounded backoff, refetches the full snapshot on every
  reconnect so nothing that changed during the outage stays stale, and
  an outage outliving the chain says so once while the loop keeps
  retrying.
- nord and tokyo-night lift `surface.dim` to clear the 3:1 readability
  floor on the chrome panel (the menu button, sidebar rows, inactive
  tabs): nord's dim leaves the published palette (#8892a8, annotated in
  the file header), tokyo-night's moves to its own dark5 (#737aa2). A
  new shipped-theme test measures the pair in every theme, so it can
  never ship unmeasured again.

### Added

- Themes own the pane ground. The workspace paints `surface.fg` on
  `surface.bg` under every pane body and maps ANSI colors 0..15 through a
  per-theme `[palette]`, so panes look like the theme instead of the host
  terminal; on a light terminal the UI used to wash white with the host's
  own text colors bleeding through. All seven shipped themes carry a
  ground and a full palette; `NO_COLOR` still leaves every host color
  untouched, and one-shot commands still print on the terminal's own
  background. A theme file that sets only `surface.fg` now counts as a
  theme, because editing it now changes the screen.
- Theme hot reload in the workspace: `cyclops theme <name>` repaints the
  open workspace on the daemon's theme event, and a hand-edited theme
  file applies on the next event, riding the existing render debounce
  with no timer.
- Pane rearranging in the workspace, one verb, swap: Ctrl+B Shift+Arrow
  swaps the focused pane with its neighbour, and dragging a pane by its
  frame onto another pane swaps the two. The frame is the drag handle so
  plain drags in the pane body stay text selection. Focus follows the
  pane the user acted on (F43). The keybinds sheet derives the swap
  chords from the live focus chords, so a rebind cannot leave it
  teaching dead keys.
- Bare `cyclops` seeds the shipped themes the same way `cyclops start`
  does, so a fresh machine's first workspace has real themes; a theme
  file that is missing (rather than broken) now says so and names the
  remedy instead of asking the user to fix a file that does not exist.
- `cyclops list` says whose roster it is: a header names the watched
  session(s) and the home that answered, and `--json` gains additive
  `home` and `sessions` fields, so a second daemon on a second home can
  never be invisible again.
- A Themes entry in the workspace menu: pick a theme with arrows and
  Enter, the active one marked; applying does exactly what
  `cyclops theme <name>` does and the open workspace repaints live.
- Swap entries in the pane right-click menu, acting on the clicked pane;
  the same swaps are optionally bindable (`swap_left` and friends) for
  anyone who wants dedicated chords.
- The theme picker previews live: moving the selection paints the
  workspace in the highlighted theme, Enter locks it in (config write
  plus daemon reload, as before), Esc restores exactly what was live
  when the picker opened. Nothing is written while browsing.
- Light mode drawn on paper instead of inverted from dark: contrast
  floor raised to 4.5:1 (WCAG AA) and every figure measured against it.
  The old file's palette.15 measured 1.01:1, white on white, which is
  what "wonky" was.
- Claude Code manifest re-verified against 2.1.221 on a live rig, with
  fidelity tests pinned from the captured evidence: mid-turn streaming,
  idle, the new trust-dialog wording, the title table, and the exact
  process triple that has to bind the manifest.

- The dashboard. `cyclops ui` on a terminal 96 columns or wider shares
  the screen with an agents panel: every watched pane, which CLI the
  daemon detects in it, its state, and how long it has stood there.
  `a` hides it; `--plain` is untouched.
- The mouse, in `cyclops ui`: click an agent to jump tmux focus to its
  pane, click a stream entry to select it, wheel scrolls. Everything the
  mouse does has a key.
- `state_ms` on every `status` pane row: how long the pane has been in
  its state, by the daemon's clock. Additive field.
- Four themes: catppuccin (Mocha), tokyo-night, nord, gruvbox, each
  mapped onto the semantic vocabulary from its upstream (MIT) palette.
  All seven shipped themes are now seeded into `~/.cyclops/themes` by
  `cyclops start`, never overwriting an edited file; installed machines
  used to list no themes at all.
- `cyclops read <t> --source detection --raw`: the pane capture the
  sensors read, in the same answer as the readings, so the evidence and
  the verdict are one moment.

### Changed

- The reply footer on delivered messages trimmed from a full flag
  synopsis to `Reply: cyclops send <from> --subject "..."`. The routing
  survives compaction and works for agents without the cyclops skill;
  the flag lesson lives in the skill and `--help`, not in every message.

### Fixed

- Three CI-red tests, all fixture-side. Two tmux fixtures targeted
  `new-window -t s`, and tmux resolves that against window names first
  with a prefix match, so an auto-renamed `sh` window pinned the new
  window to occupied index 0; the fixtures now name their windows and use
  the session-typed `s:` target. The flow-control test could stall its
  reader before the flood it measures had started flowing on a loaded
  runner; it now confirms output before the stall.
- Event panel body rows printed in the terminal's own foreground on the
  panel's themed ground, unreadable on a light host; they wear chrome
  text now.
- A theme file seeded by an old build could go stale forever: seeding
  never overwrites, so a pre-ground light.toml kept a dark background
  under a current binary on two real machines. The seeder now upgrades
  any file byte-identical to a version this project ever shipped, and
  still never touches a file the user edited.
- Pane text never falls under a 3:1 readability floor, DIM included: an agent's
  pale grays drawn for a dark terminal no longer vanish on the light
  ground, and the theme's ink no longer disappears into an agent's own
  dark fills. Readable colors pass through hue-intact; NO_COLOR never
  clamps.
- Panels painted for the wrong terminal re-ground to the theme: an
  agent that believes it is on a dark terminal (tmux reports the ground
  its real client taught it, F49) paints composer boxes and command
  bars near-black, which arrived as black boxes on the light theme. A
  neutral fill at the opposite luminance extreme from the theme ground
  now becomes the theme's own panel and the floor sets its text.
  Chromatic fills (diff greens, powerline segments), reverse video, and
  block/braille image glyphs keep their colors, and image glyphs are
  exempt from the floor too, so half-block pictures and braille plots
  render pixel-exact.
- Typed text ran past the visible pane edge into columns nobody could
  see: `window-size latest` let any attaching client out-size the
  workspace's declared canvas (F48); `window-size smallest` is the
  fixed point no client can steal, so panes never lay out wider than
  what is painted.
- Copying from scrollback grabbed the wrong rows: selection mapped
  screen coordinates straight to grid lines, which only agree when the
  pane sits at the live tail. Highlighted history now copies exactly
  what is on screen, a scrolled pane shows a dim "N back" hint on its
  frame, and a resize no longer wipes local scrollback and deadens the
  wheel. Every hydration also stops pushing one phantom blank line into
  history.
- A label held by a vanished pane blocked its name forever while
  appearing in no roster: killing a tmux session delivers no per-pane
  deaths to control mode (F47), so the adoption registry never released
  it. The daemon now frees a session's labels when tmux positively says
  the session is gone, re-verifies resurrected bindings at boot, and the
  already-taken error names the holder and the remedy that works.
- Selected text painted accent-on-raised, which measured 3.23:1 on the
  light ground, under the theme's own body-text bar; selection now
  paints panel ink on the accent ground, clearer on both themes.
- Selection copy never reached the clipboard on stock macOS: an OSC 52
  stdout write "succeeding" was treated as a completed copy, so the
  pbcopy fallback never ran and Terminal.app ignores the sequence (F44).
  The native tool now always runs when one is on PATH, with OSC 52
  emitted additionally for terminals on the near side of SSH, and the
  keybinds sheet documents select, copy, and paste.
- The cyclops skill told an agent to find its own name in the plain
  roster, which prints labels without pane ids; it now says
  `cyclops list --json` and explains what plain `cyclops read` returns.
- F25: cyclops could not see a pane die. `pane_dead` is set when the
  pane's pty fd closes, and that same closed fd is the gate that makes
  tmux skip the pane's own format subscription, so a per-pane
  subscription carrying `#{pane_dead}` can report a live pane and can
  never report the flip, on any tmux version. Cyclops only caught a death
  when an unrelated event forced a resync in the same moment, in practice
  tmux's automatic window rename: 3.6a won that race by 23ms and next-3.8
  lost it by 13ms, which is the whole of the red tmux-HEAD job. Since
  pane-dead is a delivery gate condition, a corpse could read as a live
  agent indefinitely. Fixed with one all-panes subscription, which has no
  fd gate, whose handler arms the existing debounced reconcile only when
  the pushed flag disagrees with the table; an all-live session arms
  nothing and no timer was added. Bounded at about 1s, tmux's own tick.
  Also fixed underneath it: tmux master gates `#{pane_pid}` on the same
  fd, so an empty pid made the row parser drop the whole row and read the
  death as a REMOVAL on next-3.8. An empty pid now parses as -1.
  Verified 543 tests green under both 3.6a and next-3.8.


### Added (M5: docs on the ladder, and a parity gate that keeps them true)

- `tests/e2e/parity-check.sh`: every command shape the README and docs show,
  run for real against a throwaway tmux server and checked against what
  the binaries print. 63 assertions across the six ladder rungs, the
  two-agent handoff, the open eye, and the error copy. It prints the transcript the
  README's output blocks are copied from, so a line that stops matching
  fails the script instead of quietly rotting on the page. Runs in CI
  after the test suites.
  - Isolation is a private `TMUX_TMPDIR` rather than `tmux -L`, because
    rung 1 is the first run with no config file and the config file is the
    only place a tmux socket name can be set. The default server gets its
    own directory, which nothing outside the rig can reach.
  - The stand-in agent is a shell loop with a four-rule manifest, which is
    the point of rung 3: a CLI cyclops has never heard of becomes
    addressable by adding one TOML file. It reports both turn edges
    through the real `cyclops hook` receiver, so deliveries earn
    `✔ delivered · verified` the same way a wired vendor CLI does.
  - Two negative assertions carry their own weight: `cyclops pipe` must
    NOT exist while the README says it is coming in M6, and
    `cyclops --json ui` must keep pointing machine readers at
    `cyclops watch --json`.
- README rewritten as the progressive ladder, one rung at a time: one
  pane, name panes, any terminal agent, layouts, structured messages,
  pipe. Every output block is real, captured from one parity run, with the
  home directory shortened to `~/.cyclops` and color off. The crate table
  moved out to ARCHITECTURE.md rather than being kept in two places.
- docs/guides/QUICKSTART.md: the two-agent review gate end to end. Open a `duo`
  workspace, name the panes, wire the hooks, hand work from the
  implementer to the reviewer with the sender resolved from the pane
  rather than from the request, chain the reply, read the thread back, and
  audit the pair out of the ledger file months later.
- docs/reference/MANIFESTS.md: how a new agent CLI becomes one TOML file. Every key
  of `[agent]`, `[[rule]]`, `[hooks]` and `[injection]` with what it does,
  a complete working file, and the two fields that exist for measured
  vendor quirks (`argv_basenames` for installs whose binary reports a
  version string, `line_regex_esc` for state that only the color codes
  distinguish).
- docs/reference/PROTOCOL.md: the socket, with request and response lines captured
  from a running daemon. Every method, the error codes, the ledger shape
  underneath, and the two rules a script writer trips over first: keys
  come out alphabetically, and `agent.state.report` is refused from
  anywhere but inside the pane it speaks for.
- docs/troubleshooting.md: symptom to next step, each one quoting the real
  message. Daemon down, `? unknown` panes, an open eye, deliveries that
  never verify, wait timeouts and occupant changes, a `start` that renamed
  nothing and said why, and the tests that need `--no-fail-fast`.

### Added (M5: the theme set, `cyclops theme`, and a reload that cannot half-apply)

- `cyclops theme` lists every theme it can find with a one-line swatch
  each, painted in that theme: a cell from every state group and the eye,
  with `▸` on the one that is on. `cyclops theme <name>` switches. A theme
  that will not load is refused before anything is written, and left out
  of the listing, because offering a theme that would come up as built-in
  colors is a lie the reader only finds out about later.
- The switch edits one line of `~/.cyclops/config.toml` and leaves the
  rest of the file, comments and key order included, exactly as written.
  `cyclops start` refuses to touch a config you wrote for that reason, and
  a rewrite through a TOML serializer would cost the file its comments.
- The check glyph on the answer says how far the switch got, following the
  repo's one rule for it: `✔` means cyclopsd repainted the pane borders
  and told a running `cyclops ui`, `✓` means no daemon was there to tell
  and the next command picks it up. The config is written either way.
- `theme.reload` on the daemon: no params, because it reads the config
  itself and a client-supplied name would let the two disagree about what
  is on. It repaints every adopted pane's border and emits `theme` so
  subscribers wake. The event carries the name and no colors: every
  surface resolves its own.
- resources/themes/light.toml and resources/themes/high-contrast.toml join dark.toml with
  every one of the 22 tokens set and every 256-color fallback explicit,
  and each file header now states its contrast as numbers: the ground it
  assumes, the floor every token clears against it, and how it was
  measured. high-contrast was retuned to clear 7:1 (WCAG AAA for body
  text) on every token, `state.dead` moving from #949494 (6.9:1) to
  #9e9e9e (7.8:1). A new test fails the build if a color drops below what
  its own header claims.

### Fixed (M5: themes)

- A theme file edited into a broken state could repaint a running surface
  out of the compiled default table. Only a TOML syntax error was treated
  as a failure; a file that was merely SHORTER still loaded, and every
  token it had lost fell back to a table whose lightness has nothing to do
  with the theme on screen. That is what an editor leaves behind mid-save
  (truncate, then rewrite), and what a misspelled token name leaves behind
  permanently. A reload now applies whole or not at all: it has to load
  and it has to still set every token it set before, or the colors on
  screen stay and one line says why. Choosing a different theme is exempt,
  because that palette was asked for; rewriting the key with the value it
  already had is not, which is what `cyclops theme <name>` does every
  time. Measured in findings.md as F32: rewriting a theme file the way an
  editor does, 27.3% of concurrent reads saw valid TOML defining zero of
  the 22 tokens, and none saw a syntax error, which was the only thing the
  loader used to treat as a failure.
- `cyclops theme <name>` moved the config key, and nothing running noticed.
  `ThemeWatch` stat'ed one file path, resolved once at startup, so a
  running `cyclops ui` and the daemon's pane borders stayed on the old
  palette for the life of the process. It now watches the selection: the
  config key first, because that decides which file the file check should
  even look at.
- The daemon logged the same theme warning on every repaint after a bad
  edit. Warnings are drained now (`take_warnings`), which is what makes
  "one warning line" true rather than aspirational.
- `demos/m5-theme.sh`: a switch reaching a live pane border, an edit
  reaching the same border, and a theme file caught mid-save leaving it
  exactly where it was, all read back off an isolated tmux server rather
  than off the daemon's own belief. It found the config-versus-file defect
  above while it was being written.

### Fixed (M5: docs)

- install.md documented `receipt_block_ms` as a free knob. Values above
  5000 break `cyclops send`: the CLI allows a socket read five seconds
  before it reports the connection lost, so a longer receipt budget makes
  a delivery that is going fine look like a transport failure. The ceiling
  is now written down where the knob is.
- README showed `✔ workspace ready · 1 agent` for a `cyclops start` run
  before the daemon is up. That line is the light `✓`: with no daemon to
  ask, `start` is reporting the workspace file and not a roster.

### Added (M4: naming panes, the roster, and pane border chrome)

- `cyclops name <target> <label> [--manifest <id>] [--clear]`: explicit
  pane adoption, the verb the ladder starts with. Resolves a pane id or an
  existing name, refuses reserved names, duplicates (a name is an address
  and is unique across every watched session) and control characters in a
  name, appends a `pane_labeled` system line to the session ledger, and
  paints the pane's tmux border. `--clear` un-adopts and puts the border
  back the way it was found.
- The adoption registry is durable. `src/cyclopsd/src/registry.rs`
  writes `$CYCLOPS_HOME/registry.json` whole on every change (temp file
  plus rename, 0600) and reads it at boot, so a daemon restart no longer
  silently unnames every agent. Each session reconciles its own entries
  when it attaches: an entry survives only when the pane still exists AND
  its root pid is the one it had at adoption, which is what keeps a tmux
  server restart from handing an old name to whatever inherited the id.
  A pane that closes still takes its name with it.
- `--manifest <id>` pins detection for a pane instead of working it out
  from the running process, for the wrapper scripts, `sh -c` launches and
  versioned installs where the process name lies (F21). The pin wins over
  both automatic routes, is refused when it names no loaded manifest (the
  loaded ones are listed), and binds at the moment of naming: naming
  recomputes the pane rather than waiting for the next unrelated event.
- `cyclops list`: the roster, on the strict grid. Name, state cell, and
  the pane title when the title says something the row does not already
  say. Role color on the name, group color on the state cell, glyph and
  word in both, so nothing is lost with color off. It asks `status` and
  filters to named panes: the roster has one source, not two. `--json`
  prints the same rows as pane records.
- Pane border chrome, written by the daemon on fused-state change and on
  no other edge (`src/cyclopsd/src/chrome.rs`). A named pane's border
  reads `role • state` in the theme's colors, which makes the daemon the
  third surface painting from cyclops-theme's tokens and the first that
  is not a terminal renderer. Every write is scoped and reversible:
  `@cyclops_role` / `@cyclops_state` / `pane-border-format` per pane
  (`set -p`), `pane-border-status` per window (`set -w`, the only scope
  tmux has for it, F27). The pane's prior format and the window's prior
  status are snapshotted once, at adoption, into the registry, and put
  back on `--clear`, when the pane closes, and at daemon shutdown. The
  server-global scope is never touched.
- `chrome = "on" | "off"` in `$CYCLOPS_HOME/config.toml`, on by default.
  Off writes no tmux option at all; naming still works.
- `cyclops_proto::state_words` is now the one home for the state cell's
  words, moved down out of `cyclops-ui::grid` (which re-exports it) so the
  daemon can write the same cell onto a border without linking a terminal
  UI. Two spellings of one state was the alternative.
- docs/guides/panes.md, with real output from a real run. README, install,
  themes and ARCHITECTURE updated in the same change.

### Not built, and why (M4 pane chrome)

- The daemon does not write the pane TITLE, which the brief asked for.
  Every shipped manifest reads `#{pane_title}` as a sensor (claude's
  spinner rules are the title tier at priority 1100, and a matching title
  rule means screen capture never runs at all), a title write from outside
  pushes a subscription change like any other (F13), and an agent that
  publishes its own title overwrites cyclops back inside tmux's 1Hz tick
  (F23). Writing it would replace cyclops's best evidence about a pane
  with a string cyclops wrote. The border already DISPLAYS the title by
  default, so replacing the border format replaces the view without
  touching the value: F26, and STATUS.md deviations.

### Added (M4: workspaces, layout presets, cyclops start)

- `cyclops start`: open the default workspace. Restores the saved
  workspace under that name, or builds one from a preset when there is
  nothing saved, and leaves an existing session exactly as it is, so it is
  safe to run as often as you like. Prints `✔ workspace ready · N agents`
  and, underneath, only the steps still undone: start the daemon, attach
  and start your agents, send the first message. On a first run with no
  config file it writes `~/.cyclops/config.toml` with `sessions` and
  `default_workspace`; after that the file is the user's and `start`
  prints the line to add rather than editing it. Building from a preset
  writes the workspace file too, so the next run opens a workspace instead
  of guessing at a preset; an existing workspace file is never
  overwritten. The agent count comes from cyclopsd when it is watching
  (the same roster `cyclops list` shows), from the workspace when there is
  no daemon to ask and it still describes the session pane for pane, and
  otherwise is not claimed at all: a session that has been split or closed
  since gets no names touched and a line saying so.
- `cyclops start --session <name>` opens a workspace in a session of a
  different name, which is also how a restored copy gets its names once
  cyclopsd has connected to it. Naming is impossible until then, because
  the daemon cannot resolve a pane in a session it has not attached to, so
  both verbs say which of the four states the daemon is in and print the
  exact `cyclops start` that finishes the job. A daemon that WAS asked and
  refused a name (a name is an address, unique across watched sessions)
  has its own answer printed and nothing guessed on top of it.
- `cyclops workspace save [name]` and `cyclops workspace restore [name]`:
  a session's shape as one declarative TOML file under
  `$CYCLOPS_HOME/workspaces/`. Save records windows, rows of panes, sizes
  as ratios, each pane's working directory, the names from the adoption
  registry, and anything running that is not a shell as a launch hint.
  Restore always builds a NEW session (`--session` names it), so it can
  never rearrange one somebody is working in, and it restores structure,
  not processes: panes come back empty unless `--launch` is passed, and
  even then tmux runs the recorded command as the pane's own. No keys are
  ever sent to a pane.
- Four shipped presets in `resources/layouts/`, data compiled into the binary so a
  fresh install has them before it has a config: `solo` (one agent),
  `duo` (two side by side), `quad` (even quarters), `ops` (three agents
  with the stream docked underneath). Each is the one before it plus a
  pane, and the names carry over. The `ops` dock is full width and 30% of
  the height for reasons written down in the preset and in
  docs/guides/workspaces.md: the stream does not wrap and its widest routine line
  is 59 columns, and 30% of a 48-line terminal leaves the dock a header
  plus a dozen entries while each agent keeps 33 lines.
- `cyclops_tmux::layout`: the declarative tree, `capture` off a live
  session and `apply` onto a new one, on the one-shot invocation path
  `focus_pane` already used (now shared, in `cmd.rs`). A window is rows
  top to bottom and a row is panes left to right, with every size a ratio
  of the pane cells; the model refuses what it cannot say honestly rather
  than saving an approximation, naming the window and the next step for a
  nested split and for a zoomed pane. It writes no tmux option, writes no
  pane title (that is the title sensor's input, F26), and refuses to apply
  onto a session that already exists.
- Config key `default_workspace`: what a bare `cyclops start` opens. The
  daemon recognizes it so a config carrying it does not warn, and never
  reads it, exactly like `theme`.
- Layout arithmetic measures panes, never the window. Pane chrome turns on
  `pane-border-status top`, which costs every pane a line (F27), so a grid
  checked against the window's size stopped adding up the moment a pane
  was named: `cyclops workspace save` refused a session `cyclops start`
  had built seconds earlier. A row is now a row because its panes share a
  height and span the same columns, and shares are handed out over the
  cells the panes hold.
- docs/guides/workspaces.md, and the workspace verbs in the README table and
  install page.

### Added (M4 integration: the two halves, proven together)

- demos/m4-workspace.sh is the whole M4 surface in one run, and the only
  place the two slices are exercised against each other: `cyclops start`
  builds the `duo` preset with no daemon up, `cyclops name` puts both
  names on and tmux paints the borders, `list` and `status` show the
  roster live, `workspace save` writes shape and names to a file, the
  session is killed outright, and `restore --launch` plus `start` bring
  back the panes, the ratios, the directories, the names and the chrome.
  It ends by comparing rather than asserting: the pane rectangles, the
  roster names, and a second save of the restored session are diffed
  against what was there before, and a mismatch prints the diff and exits
  non-zero, so the demo doubles as a smoke test. Isolated tmux server and
  a throwaway home, like every other demo.
- The demo also shows what border chrome costs, because a reader will
  meet it the first time they name a pane: the same two panes measured
  before and after naming, `40x24` becoming `40x23` (F27).
- ARCHITECTURE.md gains the M4 write paths. M4 is the first milestone
  that writes into tmux, on two paths that never touch: the daemon's
  chrome writes over its own control-mode connection, and the client's
  layout writes as one-shot invocations that set no option at all. The
  diagram states the invariant and where the two meet, which is the
  single call `pane.label`.

### Fixed (M4 integration)

- demos/m4-workspace.sh waited on a `--json status` pattern that could
  never match. The daemon builds its reply through `serde_json::Value`,
  whose object keys come out in ALPHABETICAL order, so
  `"name":"demo","attached":true` is not a substring of any answer it
  sends (F29). The loop fell through its 40 iterations and the demo
  carried on regardless, passing on a sleep. It now waits on a single
  field, and on a specific pane id after a restore, since "attached" on
  its own can still be answering about the session that just died.
- `build_size` in `src/cyclops/src/workspace.rs` cited F26 for the
  even-resize measurement. That is F28; F26 is the pane title.

### Added (M3: the stream UI, cyclops ui)

- src/cyclops-ui plus the `cyclops ui` verb (dispatch-only wiring in
  the CLI): the live stream. Admin view by default and deliberately calm:
  only messages addressed to admin, deliveries whose latest state is
  attention_required or parked, agents entering a blocked_* state, gate
  holds whose cause names a blocked pane, and every admin ping
  (hook-unverified notices arrive as pings). A delivery held merely
  because the recipient is mid-turn is routine and stays in the firehose.
  The firehose (tab) shows every message, delivery transition, state
  change, gate decision, and session event; a message to admin appears in
  both views.
- THE EYE in the header: `‿` closed when calm, `◑` opening at one
  attention item, `◉` open with the count beside it (glyph set documented
  with the theme tokens in src/cyclops-ui/src/theme.rs; colors ride eye.calm
  and eye.alert). Attention items are currently-blocked agents plus
  deliveries sitting in attention_required or parked_blocked_quota, keyed
  per (recipient, message) so a later message to the same agent can never
  clear an earlier one's item: only that delivery's own next transition
  does, and both those states are terminal until an operator requeues. The
  eye ticks through at most one intermediate frame per change on a single
  one-shot timer; nothing animates continuously, nothing blinks. --plain
  prints it as a word line ("eye opening · 1 needs attention").
- Rendering on the GOALS grid: an aligned HH:MM:SS gutter with hanging
  indents at the content column, role color and state glyph as the only
  meaning-carrying encodings, delivery badges byte-identical to M1
  receipts (pinned by tests against the CLI's exact strings), density
  modes (c: comfortable with body lines and breathing room, compact one
  line per entry). No reflow on arrival: autowrap is off (long lines clip,
  never wrap), pinned-to-tail scrolls, and an unpinned viewport anchors to
  an entry uid so arrivals append below it. Keys: tab view, w/f/t filter
  input mirroring the history flags, up/down/end scroll and repin, enter
  jump, c density, ? cheatsheet, q quit.
- Data: events.subscribe live push plus a one-time ledger-tail backfill
  (default 200 lines, --backfill N), merged behind a buffering intake
  that dedupes by ledger seq when one session file exists. One status
  request at startup seeds the label-to-pane map and current states.
  All IO on separate tasks feeding one channel; the event loop never
  blocks on the daemon, keypresses are handled between IO batches.
- Fluidity, measured: 10,000-entry ring with windowed rendering; frame
  build at 220x60 over the full ring is 0.33 to 0.35ms median in a debug
  build across three runs, and 0.12ms for the admin view, which filters
  the whole ring every frame; ingesting all 10,000 entries takes 3.2ms.
  Budget is 16ms, one 60Hz frame; tests/perf.rs asserts and prints both.
- Jump-to-pane: enter resolves the entry's agent through the harvested
  pane map and calls the new cyclops-tmux `focus_pane` helper (one-shot
  `tmux -u select-window` + `select-pane`, adapter-only rule intact,
  proven against an isolated tmux server).
- --plain, or a non-terminal stdin or stdout, degrades to a line-oriented
  follow mode: backfill first, then each admitted event, eye word lines,
  standard connection-loss copy and exit 1 when the daemon goes away.
  Plain mode carries the same content as the sighted comfortable view,
  message body lines included: it is the screen-reader path, so it is an
  accessibility peer rather than a reduced view.
  NO_COLOR is a color preference, not a mode: it keeps the full stream UI
  and turns the color off. Every state pairs a glyph with a word, so the
  UI is legible with no color at all, and conflating the two would have
  cost a NO_COLOR user the eye, the filters, scrolling and the jump.
  `cyclops ui --json` refuses and points at `cyclops watch --json`.
- The TUI terminal layer is hand-rolled (termios raw mode, alternate
  screen, single-write frames with per-line clears) behind a pure frame
  builder: the offline build environment carries no TUI crates, so
  ratatui/crossterm were not used; the backend is a thin seam if that
  changes.
- Tests: 42 cyclops-ui unit tests (classification, filters, eye, ring,
  selection, exact frame strings at fixed sizes, badge-voice parity with
  the CLI), the 10k fluidity measurement, 2 focus helper integration tests
  on an isolated tmux server, and 5 headless end-to-end tests driving
  `cyclops ui --plain` against a canned daemon over a scratch socket with
  a fixture ledger (calm admin stream, firehose, filter, dedupe, honest
  endings).
- Docs: docs/guides/ui.md; README ui row and crate row; ARCHITECTURE crate map
  and zero-polling notes updated to the shipped M3 client.

### Added (M3: theme engine)

- src/cyclops-theme: every color is a semantic token (role.1-8,
  surface.dim, surface.accent, eye.calm, eye.alert, five state.* and four
  badge.*, plus surface.fg as the engine's fallback for an
  out-of-vocabulary name). Themes are data-only
  TOML: values are "#rrggbb" or { hex, c256 }; an omitted 256-color
  fallback is derived (nearest cube-or-ramp xterm entry, documented and
  tested), unknown tokens warn, missing tokens fall back to a compiled
  default table (the pre-M3 CLI palette), only broken TOML rejects a file.
- The vocabulary is exactly what the renderers paint, and that now
  includes state and badge color. GOALS says color must never be the only
  encoding, which requires it to be REDUNDANT with the glyph and the word,
  not absent. M3 first read that as "states are never colored" and dropped
  the tokens; that reading was wrong and is reversed here. States and
  badges resolve five state.* and four badge.* tokens grouped by what a
  reader needs to tell apart, not one hue per state: healthy (working,
  delivered), needs-you (blocked_modal, blocked_permission, attention),
  terminal (blocked_quota, parked, the states that never retry
  themselves), quiet (idle, queued, unknown) and a dimmer dead. Role hues
  stay on the agent name alone, so the two encodings never share a cell.
  Color stays redundant and is measured that way: under NO_COLOR, --plain
  or Theme::none every state still carries its glyph and its word and
  renders byte-identically. The CLI and the stream paint from the same
  tokens through the same code, so the two surfaces cannot drift.
  stream.* (3) and surface.bg stayed dropped: nothing paints a ground, and
  the stream's gutter resolves surface.dim like every other detail column.
  Naming a dropped token warns and is skipped.
- resources/themes/: dark (the shipped default; maps the usecyclops.dev terminal
  identity, sage and mauve leading a muted eight-slot role wheel), light
  (the site's light page palette at ink strength), high-contrast (white
  and saturated grid-exact hues on the terminal's black; every value
  clears WCAG AA against it, the dimmest at 7.5:1). Each file header
  documents every mapping choice and why the absent groups are absent.
- Selection: `theme = "name"` in config.toml, `CYCLOPS_THEME` env wins
  over it; both accept a name in the themes dir (`~/.cyclops/themes`,
  falling back to `./themes`) or a direct .toml path. Hot reload for
  long-lived renderers is ThemeWatch: a (mtime, length) stat when an
  event already woke the renderer, no watcher thread, no timer; edits to
  the active theme apply on the next render.
- src/cyclops/src/style.rs resolves through the theme engine; its public
  surface (detect, none, role, accent, dim, bold, role_color) is
  unchanged and every CLI render test passes untouched. Role labels now
  hash into 8 palette slots instead of 6, so agents may land on different
  colors than before (slot count is part of visual stability going
  forward). cyclopsd recognizes the `theme` config key so a themed
  config file does not warn.
- Tests: 21 cyclops-theme unit tests (vocabulary and default table agree
  and every token resolves to its own default, every documented token is
  one a renderer paints, dropped tokens warn when named, 256-color
  derivation, parse
  tolerance, selection precedence, hot reload) plus 5 shipped-file tests
  (the three themes load with zero warnings and cover every token, role
  fallbacks stay pairwise distinct, non-role fallbacks match the
  documented derivation, high-contrast is grid-exact throughout, and
  docs/guides/themes.md's token table is pinned to the vocabulary).
- Docs: docs/guides/themes.md; docs/guides/install.md theme key;
  docs/development/ARCHITECTURE.md crate map.

### Added (M3: integration)

- demos/m3-stream.sh: the M3 surface live in one isolated rig: three
  fixture panes (implementer, reviewer, builder), two `cyclops ui
  --plain` followers capturing the admin stream and the firehose while
  the panes generate an agent-to-agent review request, a title-driven
  blocked_permission and its clear (the eye opening and closing as word
  lines), and a message to admin that lands in both views with its
  honest attention_required delivery and admin ping. A late viewer then
  backfills from the ledger tail with --with filtering, and stopping the
  daemon proves the connection-loss copy and exit 1. Twelve checks pin
  the contract in the captured logs; the full-screen TUI is the manual
  half, printed as a command to try in a real terminal.

### Added (M2: messaging read side, history + thread)

- Daemon msg.history: filter the message record (with = from-or-to,
  from/to one direction each, limit, cursor), newest last, returning
  {lines, next_cursor}. Lines are the ledger's msg/fyi facts with their
  delivery chains folded in at read time: one msg fact, N current badges;
  the files are never rewritten. Cross-session broadcasts dedupe to one
  fact with each hosting file's chain. Reading is free (any same-uid
  caller may query the whole record) and reading never writes; the name
  "me" in any filter resolves through the caller's identity envelope with
  the same fail-closed peer-credential walk msg.send uses. Reader is
  cyclops-ledger's existing read_after full scan; no indexed reader was
  added (a 10k-line ledger parses in single-digit ms, no measured need).
- Daemon msg.thread: id -> the folded msg line, every state/gate line
  sharing the id (cross-file duplicates collapse), and every msg whose
  reply_to chains to it, transitively, ordered oldest first. Unknown ids
  answer no_such_message, not an empty page.
- cyclops history [--with X | --from X --to Y | --to me] [--limit N]
  [--cursor S] and cyclops thread <id>: strict-grid rendering with a
  timestamp gutter (relative under 24h, UTC date beyond), role-colored
  from -> to, a distinct fyi column, and per-delivery badges in the M1
  receipt voice (broadcasts hang N badges under one fact line; thread
  adds bodies). --json passes the raw folded lines through; empty states
  invite the next send.
- Tests: 12 daemon unit tests over a checked-in fixture ledger covering
  every line kind (tests/fixtures/history.ndjson), 6 CLI e2e tests
  against the canned daemon, and an integration test (m2_history.rs)
  where two fixture panes exchange real messages through the daemon and
  history --with reconstructs the conversation, including the
  one-fact-N-badges broadcast read, me-resolution over the socket,
  gapless cursor walk, thread chain order, and a reboot replay.
- Docs: docs/guides/history.md; README history/thread rows.

### Added (M2: agent.wait, server-owned, plus send-and-wait)

- agent.wait rebuilt as a server-owned wait with occupant pinning:
  (pane_id, pane_pid) recorded at wait start; the pane vanishing, dying,
  or changing root pid resolves a wire error occupant_changed instead of a
  false success. Timeout is now a wire error too (code timeout), and both
  errors carry {state, waited_ms, target, until} in the new optional
  WireError.data field (additive; old clients ignore it). done tightened
  to the working -> idle edge: the current or next turn ending satisfies
  it; a blocked state mid-turn keeps waiting instead of passing as done.
  Waits are event-driven off the fusion broadcast plus the watcher stream;
  the deadline is the only timer.
- msg.send send-and-wait entries now carry {outcome, state, waited_ms,
  delivery} per recipient (outcome: reached | timeout | occupant_changed |
  not_delivered), replacing the boolean timed_out shape.
- cyclops wait <target> --until idle|done|blocked [--timeout 60s]: human
  durations (90s, 2m, 1m30s, 500ms; max 10m), badge output on reached,
  exit 0 reached / 2 timeout / 3 occupant changed. cyclops send gained
  --wait idle|done|blocked with --timeout passthrough and a wait line
  under the receipt.
- F23 (findings.md): tmux evaluates format subscriptions on a 1Hz tick;
  a title state that appears and disappears within the same second never
  produces %subscription-changed, so the title sensor's resolution is one
  second. m2_wait fixtures hold driven states across the tick.
- Tests: 6 fixture-pane integration tests (m2_wait.rs) covering each until
  mode including both done edges, timeout data, kill-pane occupant
  pinning, and a send --wait done round trip; 6 CLI e2e tests for badges,
  copy, exit codes, --json error objects, and the --wait passthrough.
- Docs: docs/guides/wait.md; docs/guides/send.md --wait section; README wait row.

### Added (M2: hooks install + startup self-test, amendment c)

- Hook config templates under resources/hooks/<cli>/ with the measured vendor
  schemas: claude settings fragment (UserPromptSubmit, Stop, Notification,
  PermissionRequest), codex hooks.json (PascalCase; no Notification event
  exists on codex), agy .agents/hooks.json (named-hooks schema, every
  event registered with a distinct self-tagging command, F7). Templates
  carry {label}/{cyclops_bin} placeholders and comment headers naming the
  trust caveats; comments are stripped at render.
- cyclops hooks install <cli> --agent <label> [--dry-run] [--dest <dir>]:
  renders to $CYCLOPS_HOME/hooks/<label>/ and prints copy-pasteable wiring
  instructions (claude --settings path; codex CODEX_HOME copy or the
  config.toml trust seed line, printed not applied, F1; agy .agents
  placement). Refuses vendor dot-dirs (.claude/.codex/.gemini/.agents)
  even via --dest.
- Daemon hook liveness (per adopted pane whose manifest declares hooks):
  every agent.state.report records a per-event last-seen edge. PaneStatus
  gained additive optional hooks_verified (skip-serialized None; old
  daemons omit it, old clients ignore it); cyclops status renders
  "hooks unverified". New socket verbs hooks.verify (tier plus last-seen
  edge ages) and hooks.selftest (one fyi marker through the normal
  delivery pipeline, subject "[cyclops] hook self-test", reporting whether
  the ack hook fired with the marker; costs one trivial turn; result is a
  ledger system line).
- F1 downgrade visibility: the first delivery that times out its tier-1
  ack window on a pane with zero hook edges ever seen emits one
  admin.notify action_required naming the likely cause (codex directory
  trust); the delivery itself resolves on screen evidence as before.
- Tests: template golden files, install e2e (dry-run, default dest, vendor
  dot-dir refusal, json mode), selftest integration with a simulated hook
  edge, and the F1 regression shape (zero-edge tier-1 pane downgrades
  cleanly, notifies once, loses nothing).

### Added (M2: commPact v1 cutover prep; prepared, never installed)

- scripts/commpact-shim/commPact: the v1 calling surface served by
  cyclops. send/read/list/resolve/doctor forward to the cyclops CLI,
  id/hash/version stay local with v1 behavior, verbs with no v2
  equivalent (type, keys, message, name) refuse honestly with exit 2,
  and a one-line deprecation note prints to stderr once per day per user
  via a stamp under $CYCLOPS_HOME.
- scripts/commpact-shim/install.sh: the guarded installer only the admin
  runs: refuses without CYCLOPS_CUTOVER_ACK=yes, moves the v1 binary to
  commPact.v1.bak (the backup IS the original), symlinks the shim, prints
  rollback, refuses on existing backups or foreign symlinks. Nothing in
  the repo executes it.
- docs/development/CUTOVER.md: the runbook: verb map, honest differences,
  preconditions, admin-only install steps, parallel window, verification
  checklist over the COORDINATION.md messaging patterns, and rollback;
  ends in ADMIN_ACTION_REQUIRED.
- scripts/commpact-shim/test_shim.py: 42 checks running the shim against
  a canned daemon on a sandbox socket, asserting verb mapping, refusals
  never reaching the daemon, stamp behavior, installer guards, and that
  the real ~/.commPact stays untouched. Python, outside cargo test; run
  python3 scripts/commpact-shim/test_shim.py.

### Added (M2: integration)

- demos/m2-conversation.sh: the whole M2 surface in one isolated rig: two
  fixture panes acting like hook-wired CLIs (acks travel through the real
  cyclops hook receiver), a send whose identity resolves from the sending
  pane, a --reply-to reply, a broadcast fyi, history --with and thread
  reconstructing the conversation, wait --until idle, hooks verify, hooks
  selftest, and jq over the session ledger.

### Fixed (M2)

- pane.read resolved strict pane ids only: cyclops read <label> answered
  no_such_target while the CLI promised "label or pane id", and the v1
  shim maps commPact read <label> onto exactly that call. The resolver
  now goes through the adoption registry first, like every other verb.

### Added (M1: delivery pipeline)

- cyclopsd delivery core per docs/development/DELIVERY.md: per-recipient FIFO workers,
  spec-order gate (no_such_pane, pane_dead, pane_in_mode, quota park-all,
  manifest modal decline or hold+notify, working/idle_with_input hold, idle
  proceeds with a forced recompute before pasting), unique cyc-<pid>-<seq>
  buffers from a 0700 spool, paste-buffer -p -d, composer verification with
  <message_id> substitution, submit, two ACK tiers (hook payload match with
  dedupe and late upgrade; screen evidence), one bounded retry, then
  attention_required plus admin notify. blocked_quota parks and never
  auto-retries.
- Ledger wired in: cyclops-ledger adopted into the workspace; one ledger per
  watched session at $CYCLOPS_HOME/ledger/<session>.ndjson. Boot, attach and
  detach, pane labeling, and admin notifications are system lines; every
  fused state change and delivery transition is a state line; gate decisions
  carry rule ids and causes only, never screen text.
- Fail-closed sender identity: socket peer (uid, pid) via LOCAL_PEERCRED or
  SO_PEERCRED, pid-ancestry walk to a watched pane_pid (labeled pane, pane
  id, or admin); nothing in a request body overrides it. cyclops-tmux pane
  rows gained pane_pid.
- New socket verbs: msg.send (receipts block up to receipt_block_ms on the
  idle path, immediate queued/parked otherwise; broadcast is one msg line
  with N delivery records), admin.notify, agent.wait, pane.label (adoption
  registry), agent.state.report (AckMatcher; unmatched reports feed fusion
  as the hook sensor).
- cyclops send: positional target merged with --to, --all, --fyi,
  --reply-to, --body/--body-file (- reads stdin); badge receipts, broadcast
  grid, exit 1 on parked/attention, 2 on usage errors. cyclops hook: silent
  exit-0 receiver posting agent.state.report with flock-serialized per-agent
  seq; failures log to $CYCLOPS_HOME/hook-errors.log.
- Config: ack_timeout_ms (1500), delivery_retry_max (1), receipt_block_ms
  (2500); unknown keys still warn, never fail.
- demos/m1-send.sh: isolated end-to-end send demo (two labeled cat panes,
  single delivery, broadcast, jq over the session ledger).
- Tests: 43 cyclopsd unit plus 9 delivery scenarios on isolated tmux -u -L
  servers validating full-ledger legality; identity unit and integration
  tests; 16 cyclops e2e covering send receipts, exit codes, and the hook
  budget.

### Fixed (M1)

- Codex idle_with_input discrimination was data-only: the manifest's
  line_regex_esc rules (typed text is bare, ghost suggestions are SGR-dim,
  F19) could never fire because nothing supplied an escaped capture, so
  typed human text read as idle and was safe to paste over. cyclops-tmux
  gained ControlClient::capture_pane_escaped (capture-pane -e), and fusion
  recompute (which the gate's fresh pre-paste evaluation runs through)
  now takes both captures whenever the bound manifest carries esc rules.
  A failed escaped capture is doubt, same as a failed plain capture,
  never an idle-biased fallback.
- No pane-rebind re-check existed between the gate's admitting recompute
  and paste/submit: a pane whose occupant changed after admit (agent
  exited to a shell, another CLI took over) got pasted into and
  Enter-submitted, and a shell occupant would EXECUTE the message text.
  The inject path now re-reads the pane immediately before the paste and
  again immediately before the submit key, requiring the pane to exist,
  be alive, keep its admitted pane_pid, and bind the admitted manifest;
  any mismatch goes to retry_queued (cause: pane_rebound) with a gate
  ledger line and the submit key is never sent (DELIVERY.md v1.1
  amendment 3).
- Deadline expiry could stand on an evidence pass that never looked: when
  the watcher was already cleared (a detach removes it before the
  lifecycle event is broadcast) or the capture failed, the tier-2 pass
  silently reported "no evidence" and an expired ACK clock returned
  Timeout, burning the attempt. Unobservable passes now freeze the
  AckClock (doubt, mirroring fusion's capture-failure handling); a
  session edge, pane activity, or a lag reconcile unfreezes it.
- A lone exact repost of an out-of-order older hook seq wiped the dedupe
  window (any replayed below-max seq read as a counter reset). Only a
  small replayed seq (<= 8, the hook restarts at 1) or three consecutive
  below-max replays read as a reset now; anything else is a duplicate.
  The (session_id, turn_id, event) dedupe stays as the backstop.
- send-and-wait omitted pane-less recipients from the wait array while
  DELIVERY.md says every recipient reports. They now get a wait entry
  carrying the resolved delivery state (attention_required) and a null
  agent state.
- Restart-limbo closure only seeded chains from msg lines via the hosted
  field, so ledgers written before that field existed (old single-file
  daemons) never closed a delivery that died before its first state line.
  A msg line with no hosted list now hosts every recipient it names.
- tests/e2e/lib/tuikit.py ran tmux without -u and without -f /dev/null
  (F14 discipline: a harness server could load the user's tmux config and
  sanitize control replies), and its dismiss_modal sent 2 to the codex
  update dialog whose measured decline is 3 (Skip until next version,
  F3). Both ported from tests/e2e/m1_soak.py; tests/e2e/test_vocab.py locks the codex
  decline.
- Detach-blind ACKs (the soak's duplicate delivery): ACK deadlines now
  freeze while a session's control connection is down and extend by the
  outage duration on reattach; reattach runs an evidence pass before any
  deadline can expire, so a delivery that landed during the outage
  resolves instead of being resubmitted; and agent.state.report resolves
  against the session's last-known pane table while detached, so hook
  ACKs no longer bounce with session_detached.
- send-and-wait ordering: the wait now starts only after the delivery
  reaches a resolved state, and until=done counts only working phases
  observed at or after this delivery's submit. Wait entries carry the
  resolved delivery state; a non-delivered resolution reports it instead
  of a fabricated wait result.
- Post-paste verification could pass on stale screen text: a generic
  verify pattern ("Pasted text") anywhere in the 15-line window, even
  from a PREVIOUS message. Generic patterns now count only on a manifest
  composer line; the substituted message id still counts anywhere.
- Tier-2 screen ACK accepted a changed composer window alone as delivery
  evidence. Per DELIVERY.md v1.1: a changed window counts only when
  verification demonstrably staged the id pattern; otherwise working or
  output evidence is required.
- Restart limbo: deliveries left in flight by a daemon stop are closed at
  the next boot as attention_required (cause: daemon_restart) with one
  aggregated admin notification. msg lines now carry a `hosted` recipient
  list so cross-session chains close only where they are hosted.
- Manifest binding silently failed on native installs whose
  pane_current_command is a bare version string (F21): binding now falls
  back to the argv[0] basename of pane_pid (ps, cached per pane+pid)
  matched against process_names plus agent.argv_basenames.
- Modal decline TOCTOU: multi-key declines re-capture the screen before
  the final confirming key and abort back to the gate loop (gate line
  decline_aborted) when the same rule no longer matches, so the confirm
  can never land in whatever replaced the dialog.
- Hook seq counter resets (the hook restarts at 1 after file loss) no
  longer eat the agent's real reports as duplicates: a replayed
  below-max seq clears that agent's dedupe window.
- A stale hook reading can no longer pin fused state: readings age out
  (5 min TTL, checked at recompute time) and are invalidated after three
  consecutive contradicting rules-tier verdicts.
- Deliveries held in gating past gate_hold_notify_ms (new config knob,
  default 120000) ping the admin once so a wedged hold is visible.
- Unresolvable-recipient state lines went to session 0 regardless of the
  sessions carrying the msg line; they now land in every involved session
  file, keeping each per-session ledger a complete stream.
- A loaded tmux buffer lingered server-global (payload included) when
  paste-buffer failed after load-buffer succeeded; it is now deleted best
  effort. cyclopsd also retired its duplicated spool logic for the
  adapter's ControlClient::load_buffer spool path under
  $CYCLOPS_HOME/spool.
- Event subscribers were dropped after ~2.5s of stall at soak rate (1024
  buffer): the event buffer is now 8192 so briefly-stalled clients
  survive; truly wedged clients still lag out and are dropped.
- Amendment i landed: injection is behind the `Injector` trait
  (paste/submit/capture) with the tmux paste path as its first
  implementation, so a headless protocol backend can slot in per agent
  without touching the gate, verification, or ACK layers.
- Delivery state watch used watch::Sender::send, which drops the value when
  no receiver is subscribed; broadcast receipts subscribed late and waited
  out the full receipt cap on already-resolved deliveries. send_replace
  stores unconditionally; broadcast receipts return as soon as every
  delivery resolves.
- tmux control connection dropped under a busy Claude TUI (8x in 80s in the
  m1 soak, blinding both ACK tiers each time): the control reader decoded
  the stream as UTF-8 lines, but pane bytes >= 0x80 ride %output verbatim
  and a split multi-byte character makes single lines invalid UTF-8 (F22).
  The reader now reads byte lines end to end; %output/%extended-output
  data is byte-faithful, reply-block text degrades lossily, and reply
  timeouts stay command-level failures that never tear the connection
  down. Regression: cyclops-tmux tests/control_load.rs holds zero
  Disconnected events through a 60 s braille/title-churn/split-sequence
  soak with concurrent command traffic.
- Control client shutdown could silently skip detach-client on an
  already-closed pipe and then wait a blind 2 s grace for a child that was
  wedged flushing stdout. The detach write is now bounded, and a child
  that never got the detach is killed without the grace wait.
- ControlClient::load_buffer wrote payload files with default permissions
  in the shared system temp dir. Spool files are now exclusive-create
  0600, optionally under a caller-supplied 0o700 spool dir
  (ControlConfig::with_buffer_spool_dir), so cyclopsd can retire its
  duplicated spool logic.

### Added (M0: shadow daemon)

- cyclops-tmux: control-mode client with FIFO reply correlation, pause-after
  flow control at attach, and a zero-polling reconciling pane watcher built
  on refresh-client -B subscriptions (probed on tmux 3.6a). All tmux access
  passes -u after finding F14.
- cyclopsd: read-only shadow daemon: config, sensor fusion over manifest
  rules (title + screen, observable disagreement), NDJSON socket server with
  ping/status/pane.read/events.subscribe, peer-credential capture, clean
  signal shutdown.
- cyclops: status/ping/read/watch with strict-grid rendering, semantic color
  slots with truecolor/256 fallback, NO_COLOR and --plain support.
- cyclops-ledger: crash-safe append-only writer (fsync, torn-tail sealing,
  monotonic seq across restarts) and cursor replay reader.
- Python probe harness ported from the validation campaign; demos/m0-status.sh
  end-to-end demo; docs/development/ARCHITECTURE.md,
  docs/development/DELIVERY.md, docs/development/GOALS.md.
- Milestone workflow queue (.claude/workflows/m1..m6) with preflight gates.
- findings.md F13-F18 (subscription probe, tmux -u locale sanitization,
  %extended-output switch, %begin flags correlation, bracketed-paste
  conditionality, macOS SO_RCVTIMEO EINVAL).

### Added (scaffold)

- Workspace scaffold: cyclops-proto (protocol v1 + ledger schema),
  cyclops-manifest (detection manifests with modal decline actions),
  cyclops-tmux (version probe), cyclopsd and cyclops binary stubs.
- Shipped detection manifests for Claude Code, Codex CLI, and Antigravity
  CLI, seeded from the 2026-08-01 validation campaign.
- CI: fmt, clippy, tests on ubuntu/macos, advisory tmux-HEAD job.
- docs/development/GOALS.md: the admin-set quality bar.
