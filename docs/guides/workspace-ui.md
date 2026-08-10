# The workspace UI

Bare `cyclops` on a TTY opens the full-screen terminal workspace: a
project sidebar, a tab bar, and a live pane canvas fed by tmux control
mode. The tab bar shows by default however many tabs the workspace has,
because the `+` that makes the next tab lives there; the app menu's
`Tab bar` item is what puts it away and brings it back. The sidebar is
the one side panel: the workspace and agent tree, over a file tree of
what is on disk. It collapses out of the way with `Ctrl+B` `b` or with
the `◂` chevron on its outer edge, leaving a one-column rail whose `▸`
chevron brings it back. The collapse and the tab bar's visibility both
persist, so the workspace reopens the way you left it.
With no tmux server running (or no sessions on it), it starts one — a
fresh session named `main` with a single shell pane in the directory you
ran it from. Its first tab is `1`; automatic tab names continue with `2`,
`3`, and so on. No preset or manual `tmux new` required; `cyclops start`
remains the front door for preset-built workspaces.

It starts cyclopsd too, when none is answering. Everything the workspace
shows about an agent — the detected name, the status glyph, the pane
chrome — comes from the daemon, so a workspace without one is a workspace
where nothing is ever detected and no state ever changes. The sidebar says
`cyclopsd offline` for as long as that is true.

```bash
cyclops                   # workspace (TTY required)
cyclops watch             # stream TUI (formerly `cyclops ui`)
cyclops watch --json      # machine-readable event stream
cyclops watch --plain     # line-oriented stream, no screen takeover
```

`cyclops ui` still works and prints a deprecation note; use `cyclops watch`.

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
| `Ctrl+B` `g` | Put the keyboard in the file panel; Esc gives it back |
| `Ctrl+B` `s` | Send a message to an agent |
| `Ctrl+B` `?` | Open the scrollable keybinding reference |

Hiding the tab bar ships with no chord: the app menu's `Tab bar` item is
the way, so a hidden strip is always something you chose. Bind
`toggle_tab_bar` in `config.toml` if you want a key for it.

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
and de-duplicated the same way creation is. Renaming a workspace by hand —
`Ctrl+B` `W` or the right-click menu — hands the name back to you permanently;
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
| `○`   | idle — safe to send a message |
| `●`   | working — a turn is running, or the composer holds staged text |
| `⚠`   | needs attention — the daemon's attention register has an open item for this pane |
| `✕`   | dead — the pane's process exited |

Sidebar rows and inactive pane borders are compact surfaces: they show the
bare glyph, with no word alongside it and no word substituted when one
doesn't fit — the glyph alone is the encoding there. The focused pane's
border pairs the glyph with its word, for example `⚠ needs attention`, when
there is room for both; dialogs and the event stream always have that
room — a narrow sidebar wraps a stream row rather than dropping its word.
The glyph itself never changes meaning: it renders identically under every
theme and under `NO_COLOR`, so only the surrounding color, never the glyph,
depends on color. Cyclops never writes the pane title to show any of
this—the title remains a sensor. See [panes.md](panes.md) for naming and
identity rules.

Pane content still maps one terminal cell to one tmux cell. The gutter is
removed from the client size reported to tmux; it never scales or covers a
pane grid. See [themes.md](themes.md) for the `chrome.text`,
`chrome.panel`, and `chrome.raised` colors.

## Mouse

Click a pane, its border, a tab, a workspace row, or a nested agent row to
focus or switch to it. Click a workspace disclosure arrow to expand or
collapse its agent children without switching workspaces. The upper-right of
every pane's top border carries
`[|][-]` controls for split right and split down; the focused pane's controls
use the accent, while the others stay dim. They are clickable without
focusing the pane first and do not cover the child terminal's first row.

## The file panel

The sidebar is two panels in one column: who is running, above, and what
is on disk, below. A dashed rule separates them; drag it to give
either half more rows, and drag it all the way to the footer to close the
file panel and hand the whole column back to the session tree. The app
menu's `Files` item is the way back, and the only way back, because a
closed panel leaves no rule to grab.

The panel roots itself on the focused pane's directory. Under its header
is a navigation row: `..` climbs out at the left, and `◂` `▸` at the right
retrace the walk and undo that. A control with nowhere to go is painted
dim and takes no click. Click the header's folder name to jump back to the
focused pane's directory from wherever you have wandered.

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

It re-reads once a second and repaints only when something you can see has
moved: a file written into an open folder shows up on its own, one written
into a closed one does not, because nothing on screen would change. A
folder with more than 500 entries is listed short and says how many it
left out.

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

## Motion

Four things fade rather than snap: a pane border taking or losing focus,
an agent's status ink, the attention eye arriving, and a notice dissolving
over the last stretch of its life. Nothing moves or slides. In a cell grid
a slide reads as a shear, so only color travels, and the glyphs and words
are at their final value on the first frame: a fade never delays the
arrival of information.

The app menu's `Motion` item is the switch, and the choice persists under
`[workspace] motion`. Motion also turns itself off where it cannot look
right: under `NO_COLOR`, on a terminal without truecolor (an interpolated
color would band across four or five entries of the 256-cube), and on a
terminal that writes frames slower than the workspace draws them.
`CYCLOPS_MOTION=0` forces it off for one run.

Hide the tab strip from the app menu's `Tab bar` item, and bring it back
the same way. That item is the only visible switch, which is why the
menu has to stay reachable from the collapsed rail. Hiding gives the
strip's row to the pane canvas and re-declares the client size, exactly
like a sidebar collapse.

The filled `+` in the tab strip opens the new-tab dialog, whether the
workspace has one tab or ten. Type a name and use
`↵ Create`, or click that action; the tab opens in the focused pane's current
directory with that name. An empty name uses the next numeric tab name.
`Esc Cancel` creates nothing. Rename and pane-name dialogs use the same
keyboard-first, mouse-clickable action model. Destructive confirmations use
the same rule: Enter or click `↵ Confirm` to proceed, Escape or click
`Esc Cancel` to back out.

Every dialog can be moved. Press its top border or title row and drag: the
card follows the pointer and stops at the screen edge, so its action row
stays reachable. The position lasts as long as that dialog does; the next
one opens centered.

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
application menu's Keybinds item opens a padded, scrollable list generated
from the bindings that are actually active; use arrow keys, Page Up/Down,
Home/End, or the mouse wheel.

Wheel over a pane scrolls its history; new output never pulls a scrolled
viewport back to the tail. Drag a gutter to resize, drag a tab onto another
tab to reorder, or onto a workspace row to move it there. Drag workspace rows
to reorder the sidebar; drag agent rows within one workspace to reorder its
children. Both sidebar orders persist. Click-drag inside a pane selects text
and copies it on release; double-click selects a word, and triple-click
selects a line.

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

Mouse reporting belongs to the workspace chrome and selection layer in this
release; mouse-aware programs inside panes do not receive those events.
Every workspace action has a keyboard binding, and ordinary pane input and
paste still pass through.

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
workspace_order = ["main", "website"]
agent_order = ["name:implementer", "name:reviewer"]
folder_tracked = []

[workspace.bindings]
name_pane = "prefix m"
toggle_sidebar = "prefix b"
toggle_tab_bar = "prefix t"
show_keybinds = "prefix ?"
```

More: [workspaces.md](workspaces.md) for presets and save/restore;
[ui.md](ui.md) for the stream TUI.
