# The workspace UI

Bare `cyclops` on a TTY opens the full-screen terminal workspace: a
project sidebar, tab bar, and live pane canvas fed by tmux control mode.
With no tmux server running (or no sessions on it), it starts one — a
fresh session named `main` with a single shell pane in the directory you
ran it from. No preset or manual `tmux new` required; `cyclops start`
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
| `Ctrl+B` `%` / `"` | Split right / down |
| `Ctrl+B` `x` | Close pane |
| `Ctrl+B` `z` | Zoom pane |
| `Ctrl+B` `w` | New workspace (folder prompt) |
| `Ctrl+B` `[` / `]` | Previous / next workspace |
| `Ctrl+B` `e` | Toggle event panel |

Unbound keys pass through to the focused pane.

## Mouse

Click panes, tabs, and sidebar rows to focus; `+` in the tab bar opens a
tab in the current pane's directory. Wheel scrolls pane history — new
output never yanks a scrolled viewport. Right-click a pane for the context
menu; the `☰ menu` button at the sidebar's bottom opens the application
menu. Split controls sit in the upper-right corner of the focused pane.
Drag a divider to resize, drag a tab onto another tab to reorder, or onto
a sidebar row to move it there. Click-drag inside a pane selects text and
copies it on release; double-click selects a word, triple-click a line.

Rebindings live under `[workspace.bindings]` in `config.toml`.

More: [workspaces.md](workspaces.md) for presets and save/restore;
[ui.md](ui.md) for the stream TUI.
