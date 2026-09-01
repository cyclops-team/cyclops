# The workspace UI

Bare `cyclops` on a TTY opens the full-screen terminal workspace: a
project sidebar, a tab bar, and a live pane canvas fed by tmux control
mode. The tab bar is visible by default because its `+` creates the next
tab. The `Tab bar` row in Settings hides or restores it.

The sidebar contains the workspace and agent tree above the file tree. Collapse
it with `Ctrl+B` `b` or the `◂` chevron; the remaining `▸` rail restores it.
Sidebar and tab-bar visibility persist, so the workspace reopens the way you
left it.

With no tmux server running (or no sessions on it), it starts one: a
fresh session named `main` with a single shell pane in the directory you
ran it from. Its first tab is `1`; automatic tab names continue with `2`,
`3`, and so on. No preset or manual `tmux new` required; `cyclops start`
remains the front door for preset-built workspaces.

It starts cyclopsd too, when none is answering. Everything the workspace
shows about an agent, including the detected name, status glyph, and pane
chrome, comes from the daemon, so a workspace without one is a workspace
where nothing is ever detected and no state ever changes. The sidebar says
`cyclopsd offline` for as long as that is true.

```bash
cyclops                   # workspace (TTY required)
cyclops watch             # stream TUI (formerly `cyclops ui`)
cyclops watch --json      # machine-readable event stream
cyclops watch --plain     # line-oriented stream, no screen takeover
```

`cyclops ui` still works and prints a deprecation note; use `cyclops watch`.

## Choose the right entry point

| Command | What opens | Use it for |
|---|---|---|
| `cyclops` | The full workspace: sidebar, tabs, live panes, files, controls, and Messages | Normal interactive work |
| `cyclops start --preset duo`, then `cyclops` | The same full workspace after constructing a named preset | A specific starting layout inside the UI |
| `cyclops start --preset duo`, then `tmux attach -t main` | The native tmux client and its configured chrome, without the Cyclops workspace UI | Headless scripts, remote sessions, or an intentionally native-tmux workflow |
| `cyclops watch` | The standalone Stream and Messages monitor, without live pane canvases | A companion dashboard or event stream |

`cyclops start` is a constructor, not the workspace renderer. It creates or
restores the session, starts the daemon when needed, names the panes it can
prove, and exits. `tmux attach` then shows that session through native tmux.

## Keyboard

Default bindings are prefix-first (`Ctrl+B`, same shape as tmux):

| Key | Action |
|-----|--------|
| `Ctrl+B` `d` | Detach (tmux session keeps running) |
| `Ctrl+B` `n` / `p` | Next / previous tab |
| `Ctrl+B` `1`–`9` | Jump to tab |
| `Ctrl+B` `c` | Open a new tab immediately with the next number |
| `Ctrl+B` `Left` / `Right` / `Up` / `Down` | Focus the pane in that direction |
| `Ctrl+B` `%` / `"` | Split right / down |
| `Ctrl+B` `x` | Close pane |
| `Ctrl+B` `z` | Zoom pane |
| `Ctrl+B` `m` | Name the focused pane / agent |
| `Ctrl+B` `,` / `&` | Rename / close tab |
| `Ctrl+B` `w` | Create a session from the focused pane's folder |
| `Ctrl+B` `[` / `]` | Previous / next session |
| `Ctrl+B` `W` / `K` | Rename / close session |
| `Ctrl+B` `b` | Collapse or reopen the sidebar |
| `Ctrl+B` `M` | Collapse or reopen the Messages pane |
| `Ctrl+B` `g` | Put the keyboard in the file panel; Esc gives it back |
| `Ctrl+B` `s` | Send a message to an agent |
| `Ctrl+B` `r` | Repaint the workspace surface |
| `Ctrl+B` `?` | Open the keybinding reference, every active binding |

Hiding the tab bar ships with no chord: the `Tab bar` row on the
settings card's View section is the way, so a hidden strip is always
something you chose. Bind `toggle_tab_bar` in `config.toml` if you want
a key for it.

`Ctrl+B` `r`, and the app menu's `Redraw` item, repaint the workspace's
own chrome from scratch. It changes nothing else: no pane, no layout, no
preference, no daemon state. It is deliberately not on a bare `Ctrl+L`,
because that belongs to whatever program is focused in the pane and is
how you redraw a garbled *pane*. This repairs a garbled *workspace*, and
taking `Ctrl+L` would have removed the pane's own repair to provide it.

Unbound keys, including modified terminal keys such as Claude Code's
`Shift+Tab`, pass through to the focused pane. Bare arrow keys are part of
that promise: every shell and agent in a pane uses them for history and
menus, so cyclops never takes them globally.

`Ctrl+B` `g` is the one exception, and it is opt-in. It puts the keyboard
in the file panel and opens the panel if it is away. While the cursor is
there, `↑`/`↓` (or `k`/`j`) move it, `→` (or `l`, or Enter) walks into a
folder or sends the file under the cursor, `←` (or `h`) climbs out, and Esc
hands the keyboard back to the pane. The prefix keeps working throughout,
so `Ctrl+B` `d` still detaches and `Ctrl+B` `b` still collapses the panel
the cursor is sitting in. Nothing else reaches the pane until you press
Esc, and the panel highlights the row that has the keyboard so it is
never a mystery where your typing is going.

Terminal paste is forwarded as one bracketed paste rather than one tmux
command per character.

Creating a workspace is immediate. Cyclops uses the focused pane's current
folder as both the new session's directory and its name, makes the name safe
for tmux, adds `-2`, `-3`, and so on when needed, then switches to it. There
is no folder prompt.

A workspace created this way keeps following its folder afterward: `cd` in
its pane and the workspace's name updates to match the new directory, sanitized
and de-duplicated the same way creation is. Renaming a workspace by hand with
`Ctrl+B` `W` or the right-click menu hands the name back to you permanently;
Cyclops never renames it out from under you again. Pre-existing sessions the
TUI didn't create, such as the default `main` session, never start following
a folder in the first place.

## What the chrome means

Workspace rows are bold and sit at the root of the sidebar; their indented
agent rows use a lighter visual weight. The selected workspace and tab have a
stronger filled background. Every pane has its own border. An inactive pane's
border uses the muted theme color; clicking a pane makes its border use the
brighter accent color immediately.

Sibling panes are separated by a compact two-cell band: one border for each
pane, with no extra blank cell between them. Split-right and split-down use
the same two-cell separation. A one-cell outer margin provides breathing room
around the canvas. The tab strip, gutters, sidebar, menus, and dialogs all use
semantic colors from the active Cyclops theme rather than a hardcoded black
background.

A pane named through Cyclops carries its identity in the top border, for
example `implementer · ● working`. The selected pane shows the full compact
state; inactive panes show the name and at most the glyph. Named panes and
unnamed detected coding agents are nested below any expanded workspace.
Manifest display names appear for unnamed agents, such as `● Claude Code`.
Clicking one switches to its workspace and tab, then focuses its pane.

### Status glyphs

The primary workspace UI intentionally omits `unknown`: the sidebar and pane
chrome show the identity alone until Cyclops has a confident state. Unknown
remains distinct from idle internally and is still available through
detection diagnostics. Every other state maps to one of four glyphs:

| Glyph | Meaning |
|-------|---------|
| `○`   | idle: no turn is running, including when the composer holds text |
| `●`   | working: a turn is running |
| `⚠`   | needs attention: the daemon's attention register has an open item for this pane |
| `✕`   | dead: the pane's process exited |

Composer text remains `idle_with_input` internally. Delivery still holds
rather than overwriting it; only the compact workspace presentation says idle.

Sidebar rows and inactive pane borders are compact surfaces: they show the
bare glyph, with no word alongside it and no word substituted when one
doesn't fit: the glyph alone is the encoding there. The focused pane's
border pairs the glyph with its word, for example `⚠ needs attention`, when
there is room for both; dialogs and the event stream always have that
room: a narrow sidebar wraps a stream row rather than dropping its word.
The glyph itself never changes meaning: it renders identically under every
theme and under `NO_COLOR`, so only the surrounding color, never the glyph,
depends on color. Cyclops never writes the pane title to show any of
this, the title remains a sensor. See [panes.md](panes.md) for naming and
identity rules.

Cyclops paints every cell your terminal gives it. The few pixels of window
padding outside the character grid keep the terminal's own color by default:
Cyclops does not change a host palette unless it can restore it exactly. To
theme that padding, set both `terminal_default_fg` and
`terminal_default_bg` under `[workspace]` as `#rrggbb` values. Cyclops then
uses OSC 10/11 to apply the theme while focused and restores that explicit
pair on every exit path, panic included, and on focus loss. A partial or
malformed pair leaves the host palette untouched. Cyclops never reads terminal
input to discover colors.

When an exact pair enabled host-palette theming, the color also follows your
focus. Switch to another tab or window of the same terminal and the configured
defaults are handed back the moment focus leaves; come back and the theme's
ground is reapplied. This rides terminal focus reporting, so a terminal
without it simply keeps the color until exit. Without that pair, only the
workspace grid is themed and there is no host palette to hand back.

Pane content still maps one terminal cell to one tmux cell. The gutter is
removed from the client size reported to tmux; it never scales or covers a
pane grid. See [themes.md](themes.md) for the `chrome.text`,
`chrome.panel`, and `chrome.raised` colors.

## Messages pane

The right-edge Messages pane is a full bordered region beside the agent
grid, not paint laid over an agent pane. Open or close it with `Ctrl+B` `M`
or its chevron. Opening it reserves its width before the agent cards are
laid out; closing it returns that width, apart from the one-column reopen
rail, and restores the exact grid that was visible before it opened. If that
grid was already narrower than the terminal because another workspace owns
the shared tmux geometry, closing Messages does not stretch it past the tmux
source.

The one-column rail remains stateful while the pane is closed. It reads only
authenticated, body-free snapshot counts and never forces the pane open:

- `✉` followed by `1` through `9` shows the current number of Work messages;
- `✉` followed by `+` means ten or more Work messages;
- `!` means at least one message notification needs attention; and
- `?` means Cyclops has no authenticated snapshot yet or the retained snapshot
  is stale after a connection gap or failed refresh.

A body-free `messages.changed` edge refreshes this cue even while the pane is
closed. Ordinary pane decoration changes do not create message reads. Opening
Messages always refreshes its detailed projection before enabling actions, and
the rail remains the same one-column click target in every cue state.

This cue is specific to the full workspace. Adopted tmux panes keep their
existing body-free message count in the pane border. A direct native tmux
attach remains intentionally free of Cyclops chrome; use the inbox commands to
inspect messaging there.

The Messages pane uses the same card language as agent panes: a complete
muted border at rest and a double accent border while it has keyboard focus.
Drag its left border horizontally to resize it. The border responds after one
cell of movement; the centered chevron remains the separate collapse control.
The resting queue keeps rows compact.
Press Enter on a message to open its authenticated detail, where the full body
and available thread history wrap to the current pane width and scroll instead
of being cut off. An inbound body is shown only after the exact recipient
claim authorizes it.
Its queue selection, detail scroll, composer, scopes, and shortcuts keep
their state as the pane opens, closes, or changes width.

Press `t` to flip the pane between the session you are looking at and every
watched session. The current-session view is addressed by that session's
durable identity together with its live panes, never by pane ids alone: tmux
hands `%1` out again after a server restart, and a message sent to the `%1`
of the `main` that died before this one belongs to that earlier session. Its
history is not lost; the all-sessions view still shows it.

When the Messages pane opens or widens, it follows the slack-first opening rule:
it consumes any unused right-side columns before shrinking agent cards. On a
follower client where the local terminal is wider than the driver-pinned tmux
window, this surplus space forms an intentional bordered peer space. Outer pane
borders extend across the slack to preserve clean visual grounding. If the
available right-side slack accommodates the Messages pane width, agent cards
remain uncompressed and cell-exact.

If local chrome leaves less room than the current tmux source, Cyclops fits the
agent card rectangles proportionally into the remaining canvas. Runtime cells
remain a 1:1 leading viewport; they are never scaled or interpolated. Opening or
closing the Messages pane never resizes the shared tmux window, from either the
sizing driver or a follower.

Pane-divider dragging is enabled whenever no fitting occurs, because local
screen coordinates match tmux source cells 1:1. When local chrome forces
proportional card fitting, pane-divider dragging is disabled because local and
tmux cell distances no longer match. The Messages pane's own width handle
remains active in all states because it adjusts local chrome rather than tmux
geometry.

Cold boot, reconnect, reconcile, and terminal resize all use the same shared
target: the agent canvas with only the collapsed one-column Messages rail
reserved. Opening or widening Messages changes the local fit, not that target;
exhausted widths derive it from the actual region left after sidebar chrome.
Reconciles follow the authoritative owner contract, querying fresh snapshots
and revalidating driver authority before modifying shared geometry.

## Mouse

Click a pane, its border, a tab, a workspace row, or a nested agent row to
focus or switch to it. Click a workspace disclosure arrow to expand or
collapse its agent children without switching workspaces.

The left end of a pane's top border carries `[▴]`, which collapses that
pane to its own title bar so only its identity shows, and `[▾]`, which puts
it back to the height it had. It appears only on a pane with another pane
stacked above or below it: two panes side by side both span the window's
full height, so neither has anywhere to put the rows it would give up, and
a control that cannot work is one this UI does not paint. A collapsed pane
is session state and is not restored on the next launch.

The upper-right of every pane's top border carries
`[|][-]` controls for split right and split down; the focused pane's controls
use the accent, while the others stay dim. They are clickable without
focusing the pane first and do not cover the child terminal's first row.

## The file panel

The sidebar is two panels in one column: who is running, above, and what
is on disk, below. A dashed rule separates them; drag it to give
either half more rows, and drag it all the way to the footer to close the
file panel and hand the whole column back to the session tree. The
`Files` row on the settings card's View section is the way back, and
the only way back, because a closed panel leaves no rule to grab.

The panel is two browsers behind one header. The agent browser follows
the focused agent: a pane switch or output from the active pane requests
one short settled snapshot of its working directory. That lets the panel
catch a `cd` without polling the filesystem. The pinned browser stays
wherever you last put it, such as a downloads folder or spec directory,
and remembers that across launches (`files_pinned_root` in `config.toml`,
written when you browse the pinned view). The chip at the header's right
end flips between them and is named for the view it switches to: `[pin]`
while you are following the agent, `[agent]` while you are pinned. A file
clicked in the pinned view still writes a reference the focused agent can
resolve: relative to the agent's own folder when the file is under it,
absolute otherwise.

Under the header is a navigation row: `..` climbs out at the left, and
`◂` `▸` at the right retrace the walk and undo that. A control with
nowhere to go is painted dim and takes no click. Click the header's
folder name to go home: the focused pane's directory in the agent view,
the saved pinned folder in the pinned one.

A folder answers a click two ways. Click its name to walk into it, which
makes it the panel's root and gives its contents the full width. Click its
chevron to open it in place instead, nested under its parent, the same
split a workspace row uses for its own disclosure marker. Only opened
folders are read, so the panel costs one directory listing over a
repository of any size. `.git` is skipped; every other dotfile is listed,
because those are the ones you edit. Symbolic links are leaves whatever
they point at, so a link back into its own parent cannot walk forever.

Files lead with their type: `(md) README.md`, `(rs) main.rs`. The tag is
there so a name the panel had to cut still says what the thing is, since
`(md) HELL…` identifies a file and `HELLO.m…` does not. Names are cut from
the end for the same reason, and a file with no extension keeps its place
in the column. On a sidebar too narrow to spend the columns, the tag drops
and the names take the space back.

Walking around the tree moves the view, not the agent. A file you click
still arrives as a path relative to the focused pane's own directory, so
`@src/main.rs` means the same thing whether you clicked it from the
project root or after walking into `src`.

Files takes a fresh snapshot after a pane route, active-pane output, or a
Files interaction, and repaints only when something you can see moved. It
does not poll the filesystem: a change made without one of those edges
appears after the next relevant interaction, route, or output. A folder
with more than 500 entries is listed short and says how many it left out.

Click a file and its path is typed into the focused pane as `@src/main.rs `,
relative to the panel's root, with a trailing space so a second click does
not run two paths together. Nothing is submitted: the path lands beside
whatever you were already typing and you say what to do with it. Every
agent in the roster reads `@path` as "this file". The notice line names
the pane it went to, since the click happened in one panel and the text
appeared in another.

Drag the sidebar's outer edge to resize it, on either tab. The edge paints
nothing at rest, so it does not stand as a second line beside the pane
canvas's own border; move the pointer onto it and a `┊` handle appears
down the whole column. The two rightmost columns of the panel answer the
grab, not just the one the handle is drawn on. The saved width is bounded
to keep both sidebar and terminal useful: at most 42 cells and never more
than half the terminal.

The `◂` chevron on the sidebar's outer edge, bottom corner, collapses
the panel; the same click as `Ctrl+B` `b`. Collapsing leaves a one-column
rail in its place carrying a `▸` chevron, and the whole column is
clickable, so the mouse always has a way back to the panel and to the
`☰ menu` button the panel carries. Every collapse and reopen hands the
remaining columns to the pane canvas and re-declares the tmux client
size, so all panes reflow.

## Settings

The app menu's `Settings` item opens the settings card: one section
showing at a time, `Tab` (and `Shift+Tab`) walking between them, or a
click on a section's chip in the card's top row. `↑`/`↓` move down the
showing section's list (a click on a row puts the cursor there;
`PgUp`/`PgDn` move eight rows, `Home`/`End` to the ends, and the wheel
moves too). Landing on a row checks it: the `✓` (the same one the app
menu's toggles wear) moves to the row, showing what `Enter` would save,
and nothing is saved yet. `Enter` (or the `Apply` button) saves what is
checked and closes; `Esc` closes and forgets it. (View and Delivery are
the exceptions: their controls apply at once and the card stays open.) The card is
one size for every section: it is sized for the longest list, so
switching sections never resizes it.

- **Theme** lists every loadable theme, the same rows `cyclops theme`
  prints. Landing on a theme checks it and previews it over the live
  workspace; `Enter` applies it exactly like `cyclops theme <name>` (the
  config key is written and cyclopsd repaints pane borders), and `Esc`
  puts the previous theme back. See [themes.md](themes.md).
- **View** lists the surfaces you can put away, `Tab bar` and `Files`,
  each with a muted line under it saying what it is. A row wears the
  `✓` while its surface is showing. These do not wait for `Apply`:
  `Enter` on a row, or a click on it, flips the surface at once, the
  card stays open, and the check follows (a chord that flips the same
  surface while the card is up moves it too). `Esc` closes the card.
  Both persist, as `[workspace] tab_bar_visible` and `files_rows`.
- **Sound** opens with what it is for, then the switch, `Sound notifs:
  on` or `off`, saved under `[workspace] sound_notifs` (off by default),
  and under a `Sounds` heading the sounds to choose from. On, the workspace plays the chosen sound when an agent
  you are not looking at gives you a reason to look: it finished a turn
  (working to idle), it needs a human (attention raised, or blocked on a
  prompt), or it died. Starting to work is silent, and so is the focused
  pane while the terminal has focus. The list is every file in
  `~/.cyclops/sounds/` by name without its extension, then
  `System alert`. `bow-ripple` (the default) and `glass-ping` ship with
  Cyclops, which `cyclops start` and bare `cyclops` seed there like the
  themes; drop your own `.wav` or `.aiff` beside them and it is listed
  next time the card opens. The
  switch and the list each have their own `✓`, and each follows the
  cursor within its group; landing on a sound plays it, and a click on
  a sound plays it again. `Enter` saves both checks at once, the switch
  as `sound_notifs` and the sound as `[workspace] sound` (`"bow-ripple"`
  by default, `"system"` for the system alert); `Esc` saves neither.
  Files play through `afplay` on macOS and the first of `paplay`,
  `aplay`, `ffplay` on Linux. `System alert` is the alert sound your
  system plays, at its alert volume: on macOS the one chosen in System
  Settings → Sound (via `osascript -e beep`), on Linux the freedesktop
  sound theme's bell. It is also what plays when the chosen file is
  missing or there is no player for it; only a system with no alert
  sound at all falls back to the terminal bell, which many terminals
  ship with the sound turned off.
- **Delivery** contains the default-off `Force staged submit` switch and its
  0 to 20 second delay. Select `Delay` and use `←`/`→` to move the slider.
  This escape hatch applies only after Cyclops has pasted an exact notification
  and ordinary verification fails. It does not paste again. At expiry the
  daemon rechecks the exact attempt and bound pane process, then reserves one
  key with `inbox.claim` before pressing Enter once. A claim, withdrawal, or
  replacement that wins before the reservation stops it, as does a successful
  disable ordered before the reservation. A later claim still retrieves the
  message, and a later setting change does not retract the reserved key. The
  warning is literal: because this bypasses composer-content proof, it may
  submit human input that appeared after the notification was
  pasted. At 0 seconds the key is attempted immediately.

`show_settings` is the binding name; `show_themes`, from when the card
was only a theme picker, still works in an existing config.

## Keybinds

The app menu's `Keybinds` item, or `Ctrl+B` `?` (`show_keybinds`), opens
the keybinding reference as a card of its own: every active binding,
chord and action, generated from the bindings actually in force rather
than from documentation, so a rebinding in `config.toml` is what it
shows. It reads only; the list scrolls (`↑`/`↓`, `PgUp`/`PgDn`,
`Home`/`End`, or the wheel, three rows a notch), and the count at the
bottom right says which rows are showing. `Enter` and `Esc` both close
it; there is nothing on it to apply.

## Motion

Four things fade rather than snap: a pane border taking or losing focus,
an agent's status ink, the attention eye arriving, and a notice dissolving
over the last stretch of its life. Nothing moves or slides. In a cell grid
a slide reads as a shear, so only color travels, and the glyphs and words
are at their final value on the first frame: a fade never delays the
arrival of information.

`[workspace] motion = false` in `config.toml` is the switch (there is no
item for it in the workspace; bind `toggle_motion` if you want a key),
and the choice persists there. Motion also turns itself off where it cannot look
right: under `NO_COLOR`, on a terminal without truecolor (an interpolated
color would band across four or five entries of the 256-cube), and on a
terminal that writes frames slower than the workspace draws them.
`CYCLOPS_MOTION=0` forces it off for one run.

Hide the tab strip from the `Tab bar` row on the settings card's View
section, and bring it back the same way. That row is the only visible
switch, which is why the app menu (and its `Settings` item) has to stay
reachable from the collapsed rail. Hiding gives the strip's row to the
pane canvas and re-declares the client size, exactly like a sidebar
collapse.

The filled `+` in the tab strip opens the new-tab dialog, whether the
workspace has one tab or ten. Type a name and use
`↵ Create`, or click that action; the tab opens in the focused pane's current
directory with that name. An empty name uses the next numeric tab name.
`Esc Cancel` creates nothing. Rename and pane-name dialogs use the same
keyboard-first, mouse-clickable action model. Destructive confirmations use
the same rule: Enter or click `↵ Confirm` to proceed, Escape or click
`Esc Cancel` to back out.

Every dialog can be moved. Its top border names the card at the left
(`╭─ Settings ─`, on the list dialogs) and carries a `[⠿]` grip at the
right, the same handle a pane frame wears. Press the border or the row
under it and drag: the card follows the pointer and stops at the screen
edge, so its action row stays reachable. The header does not light under
the pointer; the grip says it drags. The position lasts as long as that
dialog does; the next one opens centered.

`Ctrl+B` `s`, or the `@` button in the sidebar's footer, opens the
composer. The whole grammar is `@name` and then the message, taken
literally: nothing after the name is re-split, so `fix issue #42` and
`run make && test` arrive as typed. Enter sends. `Alt+Enter` breaks the
line instead (`Shift+Enter` and `Ctrl+J` do the same, for terminals that
report only one of them), and a pasted paragraph keeps its line breaks, so
a message can be as long as it needs to be. The field grows to six rows and
then scrolls its tail, keeping the cursor in view. The first line becomes
the subject every listing shows; the body keeps everything. The dialog
stays open across the send and reports the receipt where the hint was, then
leaves `@name ` in the field so a second message to the same agent is one
keystroke of setup.

If the sender identity changes while the Messages composer sends, Cyclops says
that nothing was accepted and keeps the draft. It holds message actions until
it has a fresh authoritative snapshot. Reopen the workspace after updating
Cyclops, review the unchanged draft, and send it again only after the snapshot
is current. A retained draft is not a receipt.

Right-click chooses the object under the pointer, even when it is not
active:

- a pane opens Name pane, Split right, Split down, Zoom pane, and Close pane;
- a tab opens Rename tab and Close tab;
- a workspace row opens Rename workspace and Close workspace.

The target is the pane id, window id, or session name captured by that
right-click, not a list position and not whichever item later becomes
active. Menus highlight the row under the pointer. The `☰ menu` button at
the sidebar's bottom opens the application menu. The matching `+` at the
bottom-right creates a workspace from the focused pane's folder. The
keybinding reference is the menu's `Keybinds` item (see
[Keybinds](#keybinds)).

Wheel over a pane scrolls its history; new output never pulls a scrolled
viewport back to the tail. Drag a gutter to resize, drag a tab onto another
tab to reorder, or onto a workspace row to move it there. Drag workspace rows
to reorder the sidebar; drag agent rows within one workspace to reorder its
children. Both sidebar orders persist. Click-drag inside a pane selects text
and copies it on release; double-click selects a word, and triple-click
selects a line.

A selection is anchored to the text, not to screen rows: scroll after
selecting and the highlight moves with its lines, leaves the screen with
them, and comes back when they do, and the copy is always what was
highlighted. Scrolling mid-drag grows the selection past one screen: the
viewport moves and the selection's live end follows the pointer.

A copy says what it took, where the border has room for the phrase. A
short notice (`copied 12 characters`,
`copied 4 lines`) appears on the focused pane's bottom border and clears
itself after about three and a half seconds; no keypress dismisses it. It is
painted on chrome the workspace owns, never on a pane's cells, and
nothing resizes when it appears or expires, so no agent's TUI reflows for
it. The count comes from the selection itself, which is the only honest
report available: a clipboard write can never tell Cyclops what the
terminal did with it.

To rearrange panes, drag the `⠿` grip in a pane's bottom-right border
corner onto another pane; the two swap places and the dropped pane stays
focused in its new slot. The grip is the only part of the border that
picks a pane up: clicking anywhere else on the border just focuses the
pane, and dragging the border between two stacked panes resizes them.

Mouse clicks and drags belong to the workspace chrome and selection layer.
The wheel is the exception: over a pane whose program asked for mouse
reporting, each notch is forwarded to that program as the report it
expects, which is how full-screen TUIs scroll themselves. Every workspace
action has a keyboard binding, and ordinary pane input and paste still
pass through.

Two editing chords are translated for the pane rather than passed as-is,
because no terminal can deliver them natively: `Ctrl+Backspace` arrives
in the pane as delete-word-back, and on macOS `Cmd+Backspace` arrives as
kill-to-line-start. `Cmd+A` arms the GUI gesture it starts everywhere
else: the delete pressed next clears the pane's whole input line, and any
other key forgets the arm and passes through untouched. All three chords
need a terminal that speaks the kitty keyboard protocol: legacy
encodings deliver Ctrl+Backspace as a plain backspace and Cmd chords not
at all: so the workspace requests it and degrades silently where it is
not spoken.

Workspace preferences and rebindings live in the shared `config.toml`. A
sidebar drag updates `sidebar_width` without replacing bindings or settings
owned by another Cyclops component:

```toml
[workspace]
sidebar_visible = true
sidebar_width = 28
files_rows = 8
sidebar_tab = "sessions"
tab_bar_visible = true
motion = true
sound_notifs = false
sound = "bow-ripple"
workspace_order = ["main", "website"]
agent_order = ["name:implementer", "name:reviewer"]
folder_tracked = []

[workspace.bindings]
name_pane = "prefix m"
toggle_sidebar = "prefix b"
toggle_messages = "prefix M"
toggle_tab_bar = "prefix t"
show_keybinds = "prefix ?"
```

A chord belongs to one action: a rebinding that reuses a default's chord
takes it, and the default is left unbound (`show_settings = "prefix g"`
would open Settings and leave the file panel's `g` unbound).

More: [workspaces.md](workspaces.md) for presets and save/restore;
[ui.md](ui.md) for the stream TUI.
