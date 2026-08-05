# The workspace UI

Bare `cyclops` on a TTY opens the full-screen terminal workspace: a
project sidebar, tab bar, and live pane canvas fed by tmux control mode.
With no tmux server running (or no sessions on it), it starts one — a
fresh session named `main` with a single shell pane in the directory you
ran it from. Its first tab is `1`; automatic tab names continue with `2`,
`3`, and so on. No preset or manual `tmux new` required; `cyclops start`
remains the front door for preset-built workspaces.

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
| `Ctrl+B` `e` | Toggle event panel |
| `Ctrl+B` `?` | Open the scrollable keybinding reference |

Unbound keys pass through to the focused pane. Terminal paste is forwarded
as one bracketed paste rather than one tmux command per character.

Creating a workspace is immediate. Cyclops uses the focused pane's current
folder as both the new session's directory and its name, makes the name safe
for tmux, adds `-2`, `-3`, and so on when needed, then switches to it. There
is no folder prompt.

## What the chrome means

The active workspace row and active tab have a raised background as well as
their marker or bold text. Every pane has its own border. An inactive pane's
border uses the muted theme color; clicking a pane makes its border use the
brighter accent color immediately.

Sibling panes are separated by a three-cell band: the first pane's border, a
blank themed gutter cell, and the next pane's border. A one-cell outer margin
provides the same breathing room around the canvas. Adjacent panes therefore
never fight over one shared border line. The tab strip, gutters, sidebar,
menus, and dialogs all use semantic colors from the active Cyclops theme
rather than a hardcoded black background.

A pane named through Cyclops carries its identity in the top border, for
example `implementer · ● working`. The name uses that agent's role color and
the glyph plus state word uses the state color. Named and detected agents are
also nested below the active workspace in the sidebar; clicking one switches
to its tab and focuses its pane. Cyclops never writes the pane title to do
this—the title remains a sensor. See [panes.md](panes.md) for naming and
identity rules.

Pane content still maps one terminal cell to one tmux cell. The gutter is
removed from the client size reported to tmux; it never scales or covers a
pane grid. See [themes.md](themes.md) for the `chrome.text`,
`chrome.panel`, and `chrome.raised` colors.

## Mouse

Click a pane, its border, a tab, a workspace row, or a nested agent row to
focus or switch to it. The upper-right of every pane's top border carries
`[|][-]` controls for split right and split down; the focused pane's controls
use the accent, while the others stay dim. They are clickable without
focusing the pane first and do not cover the child terminal's first row.

Drag the sidebar's right border to resize it. The saved width is bounded to
keep both sidebar and terminal useful: at most 42 cells and never more than
half the terminal.

Click `+` in the tab bar to open the new-tab dialog. Type a name and press
Enter or click `[ Create ]`; the tab opens in the focused pane's current
directory with that name. An empty name uses the next numeric tab name.
Escape or `[ Cancel ]` creates nothing. Rename and pane-name dialogs use the
same input and button model. Destructive confirmations keep No as the Enter
default: use `y` or click `[ Yes ]` to proceed.

Right-click chooses the object under the pointer, even when it is not
active:

- a pane opens Name pane, Split right, Split down, Zoom pane, and Close pane;
- a tab opens Rename tab and Close tab;
- a workspace row opens Rename workspace and Close workspace.

The target is the pane id, window id, or session name captured by that
right-click, not a list position and not whichever item later becomes
active. Menus highlight the row under the pointer. The `☰ menu` button at
the sidebar's bottom opens the application menu. Its Keybinds item opens a
padded, scrollable list generated from the bindings that are actually active;
use arrow keys, Page Up/Down, Home/End, or the mouse wheel.

Wheel over a pane scrolls its history; new output never pulls a scrolled
viewport back to the tail. Drag a gutter to resize, drag a tab onto another
tab to reorder, or onto a workspace row to move it there. Click-drag inside
a pane selects text and copies it on release; double-click selects a word,
and triple-click selects a line.

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

[workspace.bindings]
name_pane = "prefix m"
show_keybinds = "prefix ?"
```

More: [workspaces.md](workspaces.md) for presets and save/restore;
[ui.md](ui.md) for the stream TUI.
