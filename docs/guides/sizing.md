# Window sizing, and how to hand it back

A Cyclops workspace decides how big the tmux windows it shows are. It has
to: it draws a sidebar, a tab strip, and a Messages pane beside real panes,
and the panes and the chrome must agree on one geometry.

A window's size is also its panes' size, so this is not a private rendering
decision. Change it and every agent running in that session reflows.

## What the workspace does

For each window it shows, the workspace records what the window's
`window-size` was, takes the window off every sizing policy
(`window-size manual`), and moves it with `resize-window`. When it quits, it
puts every one of them back exactly as it found them.

One workspace per session acts as the authoritative sizing driver. A second
workspace on the same session follows the first one's geometry and says so
once, rather than fighting it.

### Authoritative owner reconcile contract

Ownership is tracked per session through the tmux server option
`@cyclops_window_driver`. The owner does not assume permanent authority or rely
on stale cached state across disconnections:

1. **Compare-and-set rekeying on reconnect:** When a workspace reconnects, it
   receives a new client identity. It attempts to migrate its driver claim with
   an atomic compare-and-set against its prior marker.
2. **Authority transfer detection:** If a follower detected the stale marker
   during the disconnect gap and claimed the session, the reconnected client's
   compare-and-set fails. The client yields authority, removes the session from
   its owned set, and becomes a follower without overwriting the new owner's
   window sizes.
3. **Fresh snapshot verification:** Every reconcile and post-resize pass queries
   a fresh tmux snapshot before evaluating pane dimensions or minimization
   provenance.
4. **Minimization provenance protection:** Panes with recorded minimization
   provenance (`@cyclops_pane_minimized_v1`) are re-collapsed to 1 row if a
   window resize caused tmux to automatically reflow them. Panes with no
   provenance or malformed records fail closed: manual 1-row heights are
   preserved rather than guessed or automatically uncrushed.

## Local Messages pane chrome

The Messages pane owns a bordered region beside the agent grid. It never
overlays an agent pane. Opening it reduces only this client's local canvas;
neither the sizing driver nor a follower resizes the shared tmux window when
the pane opens or closes.

Every shared size declaration, including cold boot, reconnect, reconcile, and
host resize, uses the geometry of the collapsed one-column Messages rail.
When Messages is open, Cyclops adds back only the part of its actual rendered
width beyond that rail before deriving the tmux target. It does not add back
the closed rail itself. Messages visibility and width therefore cannot change
shared pane geometry, while the closed agent grid remains cell-exact. At
exhausted widths where neither the Messages pane nor its rail fits, the same
target is derived from the actual post-sidebar region before local layout.

### Slack-first Messages opening rule

When the Messages pane opens or expands, it consumes unused right-side columns
(slack) before shrinking any agent cards:

- **Follower slack as bordered peer space:** On a follower client whose terminal
  is wider than the shared tmux layout, the surplus right-side columns form an
  intentional bordered peer space. Outer pane borders extend cleanly across the
  slack to maintain visual grounding.
- **Slack consumption:** If the available right-side slack is greater than or
  equal to the Messages pane width, the Messages pane occupies that slack
  directly. The agent cards remain completely uncompressed and retain their
  full 1:1 cell dimensions.
- **Proportional fitting:** If the requested Messages pane width exceeds the
  available right-side slack, the remaining difference proportionally fits the
  agent card rectangles into the reduced canvas.
- **Restoration on close:** Closing the Messages pane returns the width it
  reserved, apart from its one-column reopen rail, restoring the exact pre-open
  local grid without stretching it past the tmux source.

### 1:1 runtime cells and divider drag enablement

Visible pane cells always render 1:1 from the runtime's leading viewport.
Runtime cells are clipped, never scaled or interpolated:

- **Divider drag enabled:** When no fitting occurs (the local canvas
  accommodates the full tmux source layout, with Messages closed or fitting
  within available slack), local pointer coordinates match tmux source cells
  1:1. Pane divider seams act as active resize handles and can be dragged to
  resize tmux panes.
- **Divider drag disabled during fitting:** When local chrome reduces the
  canvas below the tmux source dimensions and cards are proportionally fitted,
  divider dragging is disabled because local pointer deltas no longer equal tmux
  source cells.
- **Messages width handle:** The Messages pane divider handle remains active in
  all layout states because it adjusts local UI chrome rather than shared tmux
  geometry.

At an extreme width or height where sibling content and a separating border
cannot all fit, nonfocused branches collapse so the focused pane retains
visible content and a paintable card.

**Why not let tmux decide.** Every `window-size` policy resolves votes
between attached clients, and a terminal always votes. Under `smallest` one
small terminal shrinks the session for everyone, panes and all. Under
`latest` any client can grow the window past the painted canvas and text
runs off the visible edge. `manual` has no vote to lose.

## When you need `cyclops sizing release`

A workspace that exits normally cleans up after itself and you never need
this. A workspace that was killed hard did not get the chance, and its
windows stay on `manual` with a record of the original still attached.

Any later workspace on that session repairs this on its own, because the
record lives in the tmux server rather than in the workspace that died. Use
this command when no workspace is coming back, or when you are finished with
Cyclops on a session:

```console
$ cyclops sizing release
work: cyclops sizing released
  2 window(s) put back on their original policy
```

It defaults to the session your shell is in when Cyclops uses that same
default tmux server. If `tmux_socket` selects a named server, name the target
with `--session <name>` instead; Cyclops refuses to guess a session across two
servers. Add `--json` for scripts.

A window Cyclops never sized is left exactly as it is, `manual` included,
because Cyclops did not put it there.

## When it refuses

The command refuses more often than it acts, and a refusal changes nothing
at all. Both refusals exit `3`.

**A running workspace still owns the session.** Recovery is for an owner
that is gone. A live one still holds the session and will keep sizing it, so
this refuses rather than fight it:

```console
$ cyclops sizing release
work: refused. A running workspace still owns this session's sizing
  owner: client-4821:1787795611
  Nothing was changed. That workspace puts these windows back when it quits.
  Quit it, then run this again.
```

Quit that workspace and it will restore the windows itself.

**A record cannot be read.** If the stored record of what a window was is
from a newer version, truncated, or edited by hand, the original policy is
unknowable. Cyclops will not guess one, and will not delete the record
either, because it is the only evidence of what the window was:

```console
$ cyclops sizing release
work: refused. 1 window(s) carry a record cyclops cannot read, so nothing about them was changed
  0 window(s) put back on their original policy
  @3 is still on manual and still owned. Read its record with:
    tmux show-options -w -t @3 @cyclops_prior_window_size
  then set window-size yourself and clear the record with:
    tmux set-option -w -t @3 window-size <policy>
    tmux set-option -w -t @3 -u @cyclops_prior_window_size
  finally, release the session:
    tmux set-option -t work -u @cyclops_window_driver
```

Follow those and the session is yours again.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Released. Every window Cyclops sized is back on its original policy |
| `2` | Cyclops needs an explicit `--session`: either this shell is not inside tmux, or `tmux_socket` selects a named server |
| `3` | Refused. Nothing was read or written; the message says why |
| `1` | Cyclops could not safely read its coordinator config, or tmux itself failed; the error is printed |

## The options it uses

They live in the tmux server, which is why they survive a workspace that
dies.

| Option | Scope | Holds |
|---|---|---|
| `@cyclops_window_driver` | session | Which client sizes this session, as `client_name:client_created` |
| `@cyclops_prior_window_size` | window | What the window's `window-size` was before Cyclops pinned it |
| `@cyclops_pane_minimized_v1` | pane | Provenance for deliberately minimized panes, as `v1:<original_height>` |

Reading them is safe. Changing them by hand is what the refusal messages
above ask you to do, and nothing else should.
