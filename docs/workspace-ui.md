# The workspace UI

Bare `cyclops` on a TTY opens the full-screen terminal workspace: a
project sidebar, tab bar, and live pane canvas fed by tmux control mode.

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

Click panes, tabs, and sidebar rows to focus. Wheel scrolls pane history.
Right-click a pane for the context menu. Split controls sit in the
upper-right corner of each pane.

Rebindings live under `[workspace.bindings]` in `config.toml`.

More: [workspaces.md](workspaces.md) for presets and save/restore;
[ui.md](ui.md) for the stream TUI.
