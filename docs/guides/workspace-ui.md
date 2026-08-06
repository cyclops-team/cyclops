# The workspace UI

Bare `cyclops` on a TTY opens the full-screen terminal workspace: a
project sidebar, tab bar, and live pane canvas fed by tmux control mode.
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
| `Ctrl+B` `w` | Create a workspace from the focused pane's folder |
| `Ctrl+B` `[` / `]` | Previous / next workspace |
| `Ctrl+B` `W` / `K` | Rename / close workspace |
| `Ctrl+B` `e` | Toggle event stream |
| `Ctrl+B` `?` | Open the scrollable keybinding reference |

Unbound keys, including modified terminal keys such as Claude Code's
`Shift+Tab`, pass through to the focused pane. Terminal paste is forwarded as
one bracketed paste rather than one tmux command per character.

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
there is room for both; dialogs and the event panel always have that room.
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

Drag the sidebar's right border to resize it. The saved width is bounded to
keep both sidebar and terminal useful: at most 42 cells and never more than
half the terminal.

The filled `+` in the tab strip opens the new-tab dialog. Type a name and use
`↵ Create`, or click that action; the tab opens in the focused pane's current
directory with that name. An empty name uses the next numeric tab name.
`Esc Cancel` creates nothing. Rename and pane-name dialogs use the same
keyboard-first, mouse-clickable action model. Destructive confirmations use
the same rule: Enter or click `↵ Confirm` to proceed, Escape or click
`Esc Cancel` to back out.

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
workspace_order = ["main", "website"]
agent_order = ["name:implementer", "name:reviewer"]
folder_tracked = []

[workspace.bindings]
name_pane = "prefix m"
show_keybinds = "prefix ?"
```

More: [workspaces.md](workspaces.md) for presets and save/restore;
[ui.md](ui.md) for the stream TUI.
