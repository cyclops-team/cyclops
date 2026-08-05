# Naming panes

Cyclops addresses agents by name, not by pane. `cyclops name` gives a pane
its name; `cyclops list` shows the roster.

In the full-screen workspace opened by bare `cyclops`, focus a pane and press
`Ctrl+B` `m`, or right-click it and choose **Name pane**. The dialog uses the
same daemon-backed identity as `cyclops name`, so the label immediately
becomes its message address and appears with state in the pane border and the
workspace sidebar. If `cyclopsd` is offline, the dialog stays open and shows
the error instead of pretending the pane was named.

The workspace sidebar also shows an unnamed pane when a coding-agent manifest
detects it, using the manifest's display name such as `Claude Code` or
`Codex CLI`. That is visual discovery, not adoption: the pane does not become
an address until you name it. Primary sidebar and pane chrome omit `unknown`;
`cyclops read <name> --source detection` remains the diagnostic source for the
exact fused state.

## Basics

```
cyclops name %4 reviewer          # this pane is now "reviewer"
cyclops name reviewer --self      # name the pane you are sitting in
cyclops name %4 reviewer --manifest claude
cyclops name reviewer --clear     # take the name back
cyclops list
```

`cyclops status` lists every pane cyclops watches, with its pane id.
That is where the `%4` comes from.

`--self` skips that lookup: it names the pane the command is running in,
taking the id from `$TMUX_PANE`, which tmux sets in every process it
starts. It is the form an agent uses to register itself on startup, and
the one to use when you are already sitting in the pane you mean. Outside
tmux there is no pane to name, and it says so.

## Three names a pane cannot have

```
$ cyclops name %4 admin
"admin" is you. Every message you send from a terminal is from "admin", so a pane called that could not be told apart from you on the record. Pick another name, e.g. lead.
```

`admin` is your own identity on the record. `*` addresses every agent at
once. Anything starting with `%` is a tmux pane id, which cyclops accepts
anywhere it accepts a name, so a pane called `%9` could mean two panes.
Each refusal says which of the three it is and what to use instead; none
of them is a rule you have to remember in advance.

To watch the whole thing run against an isolated tmux server, without
touching your own sessions: `./demos/m4-name.sh`.

Real output, from three named panes:

```
$ cyclops name %2 reviewer
✓ named reviewer · %2

$ cyclops list
  implementer  ● working             Implementing rate limiter
  tests        ⚠ blocked_permission  APPROVE: write to gateway.rs?
  reviewer     ○ idle                mac
```

Three columns: the name, how the agent is doing, and what it is on. The
third column is the pane title, when the title says something the row does
not already say. Agent CLIs put the current task there. So does tmux, and
`mac` above is tmux's: the title of a pane whose program never set one is
the hostname.

`cyclops status` shows the same rows plus every pane nobody has named,
and the eye. `cyclops list` is the roster alone.

`cyclops list --json` prints the same rows as pane records.

## What a name buys

A named pane is the unit everything else addresses:

- `cyclops send reviewer --subject "..."` delivers to it.
- `cyclops wait reviewer --until idle` waits on it.
- Messages FROM that pane are stamped with the name, resolved from the
  sender's process, never from what the sender claims.
- `cyclops send --all` means every named pane and nothing else.

Names are unique across every watched session, because they are
addresses. `admin`, `*`, and anything starting with `%` are reserved.

Naming is explicit on purpose. Cyclops never adopts a pane because it
looks like an agent.

## Names survive a restart

The roster lives in `$CYCLOPS_HOME/registry.json` and is read back when
the daemon starts. A name is restored only when the pane is still there
AND still runs the same root process it did when you named it, so a
tmux server that restarted and handed the id `%4` to something else does
not inherit the name. Anything that fails that check is dropped from the
file.

A pane that closes takes its name with it.

## Telling cyclops which CLI is in the pane

Cyclops works out which agent CLI a pane is running from the process, and
that is usually enough. It is not enough when the process name lies: a
wrapper script, a `sh -c`, or a versioned install whose binary reports its
version number instead of its name.

```
cyclops name %4 reviewer --manifest claude
```

The pin wins over detection and sticks with the name. `cyclops list
--json` names the manifest each pane ended up on, pinned or worked out.

Naming a pane again states the whole thing again: a `--manifest` you do
not repeat goes back to being worked out from the process.

An unknown name is refused and the loaded ones are listed:

```
$ cyclops name %4 reviewer --manifest cluade
no manifest "cluade"; loaded: agy, claude, codex
```

## The border

A named pane says so on its own tmux border. This is what tmux renders it
from, with cyclops's styling and the pane's own text already resolved:

```
$ tmux display-message -p -t %0 '#{E:pane-border-format}'
 #[fg=#ba9a91]implementer#[fg=#777777] • #[fg=#96c396]● working#[default]
```

On screen that is `implementer • ● working`, drawn into the pane's top
border.

The full-screen workspace renders the same identity as
`implementer · ● working` in its own pane chrome. Its focused border is
bright, inactive borders are muted, and the underlying tmux border remains
the durable decoration seen by ordinary tmux clients.

The name wears the agent's color and the state wears its group color, the
same two colors the same agent and state wear in `cyclops list`, in the
stream, and in `cyclops status`. Both cells carry a word, so the border
reads the same with color off.

The border is written on eight edges and no others. Nothing runs on a
clock: every write rides something that already happened, and each edge is
fired by one named function in the daemon.

| Edge | What happened | Fired by |
|---|---|---|
| adoption | you named the pane | `adopt_pane` |
| a fused state change | the agent went idle, working, or blocked | `fusion::recompute_pane` |
| a clear | you took the name back | `unadopt_pane` |
| a session attach | the daemon connected to the session | `reconcile_adoptions` |
| a window move | the pane was joined or broken into another window | `move_chrome` |
| a pane close | the pane went away | `handle_pane_event` |
| daemon shutdown | cyclopsd stopped | `restore_all_chrome` |
| a theme switch | `cyclops theme <name>` or `theme.reload` | `reload_theme` |

The last one is why a switch shows up on the border at once instead of
waiting for the agent to do something.

Turn it off in `$CYCLOPS_HOME/config.toml`:

```toml
chrome = "off"
```

Off means cyclops writes no tmux option at all. Naming still works; the
pane just does not say so.

### What it touches, and how it comes back

Four tmux options per named pane, all scoped and all reversible:

| What | Scope | Why |
|---|---|---|
| `@cyclops_role`, `@cyclops_state` | this pane | the text |
| `pane-border-format` | this pane | the color around the text |
| `pane-border-status` | this pane's window | tmux draws no border text without it |

The pane's own `pane-border-format` and the window's `pane-border-status`
are read once, before the first write, and put back on `--clear`, when the
pane closes, when the pane moves to another window (the window it left),
and when the daemon shuts down. Nothing global is touched.

Those two values exist in one place: the name's own record. tmux is
wearing cyclops's by then, and nothing else wrote yours down. So `--clear`
puts the border back FIRST and drops the name second.

If tmux refuses the write, `--clear` fails and the name stays. The error
says what is still cyclops's, names both options, and ends with the
command to run again. Run it again once tmux is answering and the original
values come back. Renaming the pane in between changes nothing: a pane
whose snapshot is already on file is never re-read, so cyclops's own
border can never be recorded as yours.

Only the last one is not per-pane, and that is tmux's doing: there is no
pane scope for it. So a window carries border text exactly while it holds
a named pane. Move a named pane to another window and the text goes with
it: the window it left goes dark, the window it joined lights up.

Border text costs the window one row, because tmux draws the border where
the text goes. That is the one visible cost, and `chrome = "off"` is the
way out of it.

Cyclops does not write the pane title. The title is where an agent
publishes what it is doing, and that is one of the signals cyclops reads
to know whether the agent is working or idle. Writing it would mean
overwriting the evidence with decoration, and the agent would overwrite
cyclops back within a second anyway. The border already shows the title by
default, so replacing the border format replaces the view without touching
what it is a view of.

## The record

Naming and clearing both append a `system` line to the session ledger:

```
$ jq -c 'select(.data.event == "pane_labeled")' ~/.cyclops/ledger/main.ndjson
{"seq":3,"boot_id":"ea198d4a-...","id":"e-2e1190","ts":1785722524757,"kind":"system","from":"cyclopsd","to":[],"data":{"event":"pane_labeled","label":"reviewer","manifest":"claude","pane_id":"%0"}}
{"seq":4,"boot_id":"ea198d4a-...","id":"e-776dba","ts":1785722524769,"kind":"system","from":"cyclopsd","to":[],"data":{"event":"pane_labeled","label":null,"manifest":null,"pane_id":"%0"}}
```

`label: null` is the clear. The registry file is a cache of the current
roster; the ledger is the history of how it got there.
