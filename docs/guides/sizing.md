# Window sizing, and how to hand it back

A Cyclops workspace decides how big the tmux windows it shows are. It has
to: it draws a sidebar, a tab strip and a Messages pane beside real panes,
and the panes and the chrome must agree on one geometry.

A window's size is also its panes' size, so this is not a private rendering
decision. Change it and every agent running in that session reflows.

## What the workspace does

For each window it shows, the workspace records what the window's
`window-size` was, takes the window off every sizing policy
(`window-size manual`), and moves it with `resize-window`. When it quits it
puts every one of them back exactly as it found them.

One workspace per session does this. A second workspace on the same session
follows the first one's geometry and says so once, rather than fighting it.

## Local chrome on a follower

The Messages pane owns a bordered region beside the agent grid. It never
overlays an agent pane. Opening it reduces only this client's local canvas;
a sizing follower does not resize the shared tmux window to make room.
Instead, the follower proportionally fits the agent card rectangles into the
smaller canvas and shows each runtime as a 1:1 leading viewport. The runtime
cells are clipped, never scaled.

Closing the Messages pane returns the width it reserved, apart from its
one-column reopen rail, and restores the exact pre-open local grid. It does
not expand that grid beyond the current tmux source. If the terminal is wider
than the source, any remaining far-right space belongs to the shared sizing
state and is resolved by the sizing owner or a later authority takeover, not
by a follower changing the window.

While cards are locally fitted, their divider seams are not resize handles:
a local pointer delta is no longer the same number of tmux source cells. The
Messages pane width handle remains active because it changes local chrome,
not tmux geometry. At an extreme width or height where sibling content and a
separating border cannot all fit, nonfocused branches collapse so the focused
pane retains visible content and a paintable card.

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

It defaults to the session your shell is in. Name another with
`--session <name>`, and add `--json` for scripts.

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
| `2` | Not inside tmux and no `--session` given |
| `3` | Refused. Nothing was read or written; the message says why |
| `1` | tmux itself failed; the error is printed |

## The two options it uses

Both live in the tmux server, which is why they survive a workspace that
dies.

| Option | Scope | Holds |
|---|---|---|
| `@cyclops_window_driver` | session | Which client sizes this session, as `client_name:client_created` |
| `@cyclops_prior_window_size` | window | What the window's `window-size` was before Cyclops pinned it |

Reading them is safe. Changing them by hand is what the refusal messages
above ask you to do, and nothing else should.
