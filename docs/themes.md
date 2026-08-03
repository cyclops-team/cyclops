# Themes

Every color Cyclops prints is a semantic token, resolved through a theme.
Code never names a raw color; a theme file maps tokens to values. The
engine lives in `crates/cyclops-theme`, and both surfaces resolve every
color through it: the one-shot CLI commands and `cyclops ui`.

## Pick a theme

In `~/.cyclops/config.toml`:

```toml
theme = "dark"
```

Or per invocation, which wins over the config:

```bash
CYCLOPS_THEME=light cyclops status
```

Both accept a name (resolved to `<themes dir>/<name>.toml`) or a path to a
`.toml` file. The themes directory is `~/.cyclops/themes` when it exists,
else `./themes` relative to the working directory (the repo layout).

Shipped themes, all on the usecyclops.dev identity: `dark` (the default),
`light`, `high-contrast`. With no theme file anywhere, a compiled default
table renders; a theme that names only some tokens falls back to that
table for the rest.

`NO_COLOR` and `--plain` win over everything: no theme is even read.

## Tokens

21 tokens change what you see:

| Group | Tokens | Used for |
|---|---|---|
| `role` | `1`..`8` | Stable per-agent colors; a label hashes to a slot |
| `surface` | `dim` | Detail columns, gutters, separators, every dimmed qualifier |
| `surface` | `accent` | The marker on the selected entry in `ui` |
| `eye` | `calm` `alert` | The eye, in both the `status` and `ui` headers: calm closed, alert open |
| `state` | `healthy` `needs_you` `terminal` `quiet` `dead` | Agent state cells, by group |
| `badge` | `healthy` `needs_you` `terminal` `quiet` | Delivery badges, the same groups read on a delivery |

One more token exists and is worth knowing about. `surface.fg` is the
engine's fallback for a token name outside this list, which only a bug
produces. No renderer paints it, so editing it changes nothing on screen.
The shipped themes set it anyway, because it names their text color.

## What the colors mean

Two things carry meaning: the color of an agent's name, and the state
glyph. Nothing else.

An agent's name is colored by role, so the same agent is the same color
everywhere. State color is a second reading of what the glyph and the
word already said, never a replacement for it. Turn color off and you
lose nothing: `● working` is still `● working`.

States are grouped, not one color each. Four groups answer the only two
questions a color is any good at answering across a room, plus one step
below them for a pane that is gone.

| Group | States | Answers |
|---|---|---|
| `healthy` | `working`, delivered | Doing its job |
| `needs_you` | `blocked_modal`, `blocked_permission`, needs attention | Yours to clear |
| `terminal` | `blocked_quota`, parked | Yours to clear, and it will never clear itself |
| `quiet` | `idle`, `idle_with_input`, `unknown`, queued, and every in-flight delivery step | Nothing to do |
| `dead` | `dead` | The pane is gone (state cells only) |

Role color lands on the agent's name and state color on the state cell,
so no cell carries both. `NO_COLOR` and `--plain` read the same as a
colored terminal, word for word.

A state cell reads the same in `cyclops status` and in the stream, and a
delivery badge reads the same on a receipt, in `cyclops history` and in
the stream. Both surfaces compose the cell in one place
(`crates/cyclops-ui/src/grid.rs`) and supply only the paint, so a theme
edit moves them together.

## What is not themeable

There are no `stream.*` tokens. The stream's gutter resolves
`surface.dim` like every other detail column, and subjects and bodies
print in the terminal's own foreground, so tuning `surface.dim` moves the
CLI and the stream together instead of letting them drift.

There is no `surface.bg`. Nothing paints a ground: one-shot commands
print onto the terminal's own background and the full-screen stream
inherits it.

Naming either of them in a theme file warns on stderr and is skipped, and
so does a per-state token name from before the groups (`state.idle`,
`badge.attention`):

```
theme: unknown token `stream.gutter` ignored
```

## File format

Data-only TOML. A value is either `"#rrggbb"` or a table with an explicit
256-color fallback:

```toml
name = "dark"

[surface]
dim = "#777777"                        # fallback derived
accent = { hex = "#b7c396", c256 = 144 } # fallback explicit
```

Unknown tokens warn and are ignored; missing tokens fall back to the
compiled defaults; only broken TOML rejects the file. Nothing in a theme
executes.

When `c256` is omitted it is derived: nearest xterm-256 entry by squared
RGB distance, comparing the 6x6x6 color cube (16..231) against the
grayscale ramp (232..255); ties go to the lower level and the cube. The
base 16 colors are never derived because terminals remap them freely. The
shipped themes write every fallback out explicitly; the role slots in
`dark.toml` are hand-tuned where the derivation would collapse two muted
hues onto one entry, since two roles must never share a color.

## Editing a live theme

A change to the active theme file applies on the next render: the engine
re-checks the file (a stat of mtime plus length) when something is about
to repaint, with no watcher thread and no timer, keeping the zero-polling
contract. One-shot commands read the theme at startup, so they always
print the current file.

A file broken mid-edit behaves differently on the two surfaces, because
they hold the theme differently. A running `cyclops ui` keeps the colors
it already has and prints nothing. A one-shot command has no previous
colors to keep, so it falls back to the built-in table and says why on
stderr:

```
theme: theme ~/.cyclops/themes/dark.toml isn't valid TOML: ... Using built-in colors.
```

Deleting the file falls back to the built-in table on both surfaces.
That is silent for the default theme. A theme you chose explicitly, with
`CYCLOPS_THEME` or the `theme` config key, warns that it resolved to
nothing.
