# Themes

## Pick a theme

```bash
cyclops theme            # what is there, and what each one looks like
cyclops theme light      # switch
```

```
  blossom        ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  buttercream    ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  catppuccin     ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
▸ dark           ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  ember          ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  forest         ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  gruvbox        ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  high-contrast  ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  light          ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  meadow         ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  midnight       ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  nord           ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  obsidian       ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  periwinkle     ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  seafoam        ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  sorbet         ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  tokyo-night    ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉

  cyclops theme <name> to switch
```

Each row is painted in its own theme, and `▸` marks the one that is on.

Only files that would change a color are listed. A file that will not
parse, and a file that parses and sets no token at all, are both left out:
a row here is an offer to switch, and either one would switch you to the
built-in colors under somebody else's name. Typing such a name is refused
for the same reason. See [what counts as setting nothing](#file-format).

Switching writes `theme = "light"` into `~/.cyclops/config.toml` and
leaves the rest of that file alone, comments included.

```
✔ theme light
  light  ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
```

The first line says how far the switch got, and it is the only line that
claims anything about the screen.

`✔` is cyclopsd answering that it is painting `light` now: the pane
borders and any running `cyclops watch` are already on it.

`✓` means no daemon was running to ask. The config is written either way,
and the line says `the next command picks it up`.

`⚠` means a daemon answered and is painting something else. The config is
written and the next one-shot command is on the new theme, but the pane
borders are not, and no later command moves them:

```
⚠ theme light · saved, not live
  light  ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉
  cyclopsd is still painting dark, so pane borders did not change. Check CYCLOPS_THEME and the themes directory where cyclopsd runs, then restart it.
```

Two things put it there, and restarting cyclopsd clears either one once
the cause is gone. `CYCLOPS_THEME` in the daemon's environment beats the
config key and is fixed for the life of that process, and a bare name
resolves against `./themes` relative to the daemon's own working directory
when `~/.cyclops/themes` does not exist.

Seventeen themes ship. Three are the usecyclops.dev identity: `dark` (the
default), `light`, `high-contrast`. Four are loved terminal palettes
mapped onto the same vocabulary: `catppuccin` (Mocha), `tokyo-night`,
`nord`, `gruvbox`; each names its upstream project (all MIT) at the top
of its file. Ten are this project's own rather than ports, so they carry
no upstream credit: six lights drawn on tinted paper (`blossom`,
`buttercream`, `meadow`, `periwinkle`, `seafoam`, `sorbet`) and four
darks (`ember`, `forest`, `midnight`, `obsidian`). Each file header says
what its theme is for.
`cyclops start` seeds all seventeen into `~/.cyclops/themes` and never
rewrites one you edited.
A home seeded before `surface.bg` and `palette` existed keeps working:
those tokens resolve from the compiled defaults until you delete the
file and run `cyclops start` to reseed, or add them by hand.

Two other ways to choose, for a config you maintain by hand and for one
run:

```toml
theme = "dark"           # ~/.cyclops/config.toml
```

```bash
CYCLOPS_THEME=light cyclops status
```

`CYCLOPS_THEME` wins over the config key, which wins over the default.
Both accept a name (resolved to `<themes dir>/<name>.toml`) or a path to a
`.toml` file. The themes directory is `~/.cyclops/themes` when it exists,
else `./themes` relative to the working directory (the repo layout). With
no theme file anywhere, a compiled default table renders; a theme that
names only some tokens falls back to that table for the rest.

`NO_COLOR` and `--plain` win over everything: no theme is even read.

## Tokens

42 tokens change what you see:

| Group | Tokens | Used for |
|---|---|---|
| `role` | `1`..`8` | Stable per-agent colors; a label hashes to a slot |
| `surface` | `fg` | The pane text the workspace paints on `surface.bg`; `surface.fg` doubles as the engine's fallback for a token name outside this list |
| `surface` | `bg` | The pane ground: the workspace paints it under every agent pane; one-shot commands still print on the terminal's own background |
| `surface` | `dim` | Detail columns, gutters, separators, every dimmed qualifier |
| `surface` | `accent` | The marker on the row a surface is pointing at: the selected entry in `watch`, the active theme in `cyclops theme`, the workspace's focused-pane ring |
| `eye` | `calm` `alert` | The eye, in both the `status` and `watch` headers: calm closed, alert open |
| `state` | `healthy` `needs_you` `terminal` `quiet` `dead` | Agent state cells, by group, including on pane borders |
| `badge` | `healthy` `needs_you` `terminal` `quiet` | Delivery badges, the same groups read on a delivery |
| `chrome` | `text` `panel` `raised` | The workspace chrome palette: explicit `text` on `panel` under the tab strip, pane gutters and menus; `raised` under the active tab, active workspace row, hovered menu items and selected text |
| `palette` | `0`..`15` | The ANSI-16 mapping for pane content: the color a pane's program gets when it asks for terminal colors 0..15 |

## What the colors mean

Two things carry meaning: the color of an agent's name, and the state
glyph. Nothing else.

An agent's name is colored by role, so the same agent is the same color
everywhere, including on its pane border. State color is a second reading
of what the glyph and the word already said, never a replacement for it.
Turn color off and you lose nothing: `● working` is still `● working`.

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
(`src/cyclops-ui/src/grid.rs`) and supply only the paint, so a theme
edit moves them together.

## Contrast

A contrast ratio is how far a color stands out from the background behind
it, from 1:1 for invisible up to 21:1 for black on white. The numbers here
are the ones the accessibility guidelines for the web use (WCAG 2.1,
measured on relative luminance in sRGB), because they are the only
published bar for "can a person read this".

Three themes publish a floor. Their file headers state the ground, the
`surface.bg` they ship, and the ratio every token clears against it. The
bars below are the bars, not a summary of them:
`shipped_themes_meet_their_stated_contrast` in
`src/cyclops-theme/tests/shipped.rs` measures every color against them,
and `the_published_bars_are_the_bars_that_get_measured` fails the build if
this table and that test stop agreeing. A retuned color that drops below
what is published here fails too.

| Theme | Ground | Every token clears | Except |
|---|---|---|---|
| `dark` | `#0d0d0d`, its `surface.bg` | 4.3:1 | `state.dead`, 2.8:1 |
| `light` | `#fefefe`, its `surface.bg` | 4.5:1, WCAG AA for body text | `state.dead`, 2.8:1 |
| `high-contrast` | `#000000`, its `surface.bg` | 7:1, WCAG AAA for body text | nothing |

Every number is a floor. `state.dead` gets its own because it is
deliberately the hardest cell to read: it marks a pane whose process is
gone, and there is nothing to do about one. Both exceptions measure
2.82:1 against their ground.

The other fourteen publish nothing here, on purpose. A bar in a header
is a promise something measures; a bar written to describe colors that
already exist measures nothing and reads exactly the same. What every
one of the seventeen is held to instead is structural and checked for
all of them: the file loads with zero warnings, sets all 42 tokens,
keeps its eight role fallbacks and its sixteen palette entries pairwise
distinct, and clears the `surface.dim` bar below.

`surface.bg` and the two `chrome` grounds are backgrounds, not figures,
so they are not held to the figure floor. The chrome bar, measured by
the same test, is that `chrome.text` stays readable on both grounds at
the theme's floor, and that the grounds stay different in truecolor and
256-color terminals. Chrome's dimmed ink has its own bar: `surface.dim`
on `chrome.panel` (the sidebar, the menu button, inactive tabs) clears
3:1 in every shipped theme, the same `MIN_CONTRAST` floor the workspace
holds pane cells to.

`palette.*` has no bar at all: palette entries are content colors the
running program picks, so the theme only supplies the mapping
(`palette.0` on a dark ground could never clear a floor).

## What is not themeable

There are no `stream.*` tokens. The stream's gutter resolves
`surface.dim` like every other detail column, and subjects and bodies
print in the terminal's own foreground, so tuning `surface.dim` moves the
CLI and the stream together instead of letting them drift.

`surface.bg` and the `palette` group ground only the workspace: it
paints `surface.bg` under every agent pane and maps ANSI colors 0..15
through `palette`. One-shot commands are the surface that stays
unthemeable here: they print onto the terminal's own background, the
CLI has no background emitter, and the full-screen stream inherits the
terminal's ground the same way.

While the workspace runs it paints every terminal cell with the theme. The
terminal's own window padding stays at its native color unless you configure
an exact restoration pair. Cyclops never reads terminal input to discover
those colors: the workspace input thread is their only owner. To theme the
padding as well, configure both exact defaults yourself:

```toml
[workspace]
terminal_default_fg = "#3a2b26"
terminal_default_bg = "#000000"
```

With a complete valid pair Cyclops applies the theme through OSC 10/11 while
focused and restores those colors through the same sequences on focus loss and
exit. With either value missing or malformed it leaves the host palette
untouched. The UI preserves these operator-owned keys when it saves its other
workspace preferences.

Naming a `stream.*` token in a theme file warns on stderr and is
skipped, and so does a per-state token name from before the groups
(`state.idle`, `badge.attention`):

```
theme: unknown token `stream.gutter` ignored
```

## File format

Data-only TOML. A value is either `"#rrggbb"` or a table with an explicit
256-color fallback:

```toml
name = "dark"

[surface]
dim = "#777777"                          # fallback derived
accent = { hex = "#b7c396", c256 = 144 } # fallback explicit
```

Unknown tokens warn and are ignored; missing tokens fall back to the
compiled defaults; only broken TOML stops the file loading at all. Nothing
in a theme executes.

That tolerance has one edge, and `cyclops theme` handles it rather than
letting you fall into it. A file that loads and sets none of the 42 tokens
that change what you see is not listed and not accepted by name. Empty, a
`name` and nothing else, and every token name stale all parse cleanly,
and every color would still come off the compiled table, so choosing one
would repaint nothing anywhere:

```
$ cyclops theme stale
theme: unknown token `state.idle` ignored
can't use theme "stale": ~/.cyclops/themes/stale.toml sets no colors, so switching to it would change nothing on screen. Nothing was changed. Pick another with cyclops theme.
```

Setting one token is enough. The rule is about a file that sets none, not
about a sparse one.

When `c256` is omitted it is derived: nearest xterm-256 entry by squared
RGB distance, comparing the 6x6x6 color cube (16..231) against the
grayscale ramp (232..255); ties go to the lower level and the cube. The
base 16 colors are never derived because terminals remap them freely. The
shipped themes write every fallback out explicitly; the role slots in
`dark.toml` are hand-tuned where the derivation would collapse two muted
hues onto one entry, since two roles must never share a color. The
`palette` fallbacks are the literal ANSI index in every theme: a program
that asked for color N gets entry N on a 256-color terminal.

## Editing a live theme

Save the file. The change applies on the next thing that repaints.

Long-lived surfaces (`cyclops watch`, the pane borders cyclopsd writes) hold
the selection and re-check it when an event has already woken them: a
stat of the config key and of the theme file, no watcher thread and no
timer, so the zero-polling contract holds. One-shot commands read the
theme at startup, so they always print the current file.

A reload applies whole or not at all. The file has to load, and it has to
still set every token it set before. Otherwise the colors already on
screen stay and one line says why:

```
theme: ~/.cyclops/themes/dark.toml stopped setting `surface.dim` and 4 more. Keeping the colors on screen. Fix the file and save again.
```

That rule is there for the way editors save. Truncate, then rewrite: a
stat landing in the middle reads a shorter file that is still valid TOML,
and loading it would paint every missing token out of the compiled table,
whose lightness has nothing to do with the theme you are on. A misspelled
token name does the same thing and stays until you notice. Neither one
reaches the screen.

`cyclops watch` shows that line on its notice row. One-shot commands print it
on stderr; they have no previous colors to keep, so a file they cannot
read falls back to the built-in table and says so:

```
theme: ~/.cyclops/themes/dark.toml isn't valid TOML (line 1, column 9: invalid table header; expected `.`, `]`), so cyclops is using built-in colors until you fix it.
```

Choosing a *different* theme is the one case that is exempt: you asked for
that palette, so a theme that sets only a few tokens takes the compiled
defaults for the rest, exactly as it would on a fresh start. The exemption
is about the file changing, not about the config changing, so
`cyclops theme light` while you are already on light is still an edit and
still held to the rule.

That case is also the one the switch's `✔` cannot speak for. The check
reports which theme cyclopsd says it is painting, and a refused edit
leaves it painting the same theme under the same name, so re-choosing the
theme you are already on prints `✔` whether or not the edit applied.
cyclopsd logs the refusal. Choosing a different theme always applies
whole, so there the check answers for the whole of what it claims.

`demos/m5-theme.sh` runs all of it against a throwaway tmux server: a
switch reaching a live pane border, an edit reaching the same border, and
a theme file caught mid-save leaving it exactly where it was.
