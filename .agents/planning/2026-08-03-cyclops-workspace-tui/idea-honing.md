# Idea Honing: Cyclops Terminal Workspace UI

Requirements clarification Q&A for the full-screen Cyclops workspace TUI.
Rough idea: `rough-idea.md` (copied from `docs/rough-idea.md`).

## Question 1

For the first usable release, what level of live terminal-pane fidelity is
required?

**Answer:** Option 2 — composite renderer with a deliberate feature floor.
Cyclops owns all rendering (tmux control mode feeds a VT emulator, likely
`libghostty-vt`; same architecture as Herdr's rendering layer but fed by tmux
instead of owned PTYs). Ship when common ANSI/VT is solid: text, full color
(16/256/truecolor), attributes, cursor, alternate screen, keyboard forwarding,
basic scrollback. Defer the long tail: image protocols (Sixel/Kitty graphics),
mouse forwarding into pane programs (chrome mouse still works), exotic escape
sequences. Alternatives considered: full fidelity from day one (Option 1,
rejected as too much scope before workspace features exist) and native tmux
rendering with limited chrome (Option 3, rejected — can't deliver the custom
chrome that motivates the project).

## Question 2

When the workspace UI is not running (crashed, closed, or never opened), what
must still be true? Concretely: is the UI a pure optional client over the
existing daemon + tmux, or does any new state (workspace/tab organization,
pane arrangement metadata) need to live somewhere durable that other Cyclops
commands can see?

**Answer:** Durable and shared. Workspaces map to tmux sessions and tabs map
to tmux windows, so the organization lives in tmux itself and survives the UI
closing or crashing — reattaching with plain `tmux attach` shows the same
structure. Metadata tmux cannot express (display names, agent identity,
states) already lives in the daemon and ledger; any new workspace metadata
follows the same pattern so every client sees the same answers. Rejected: a
UI-private layout file (second source of truth that drifts from tmux) and
ephemeral organization (contradicts the persistence goals in the rough idea).
Constraint accepted: workspace/tab operations are bounded by what tmux
allows, per the tmux-action-map research.

## Question 3

Which chrome features are in the first usable release, and which can wait?
The rough idea lists: workspace sidebar, application menu, tab bar, pane
titles/borders, split controls, pane context menu, drag-to-rearrange
(tab reordering, pane drag between splits), mouse resizing, agent
state/attention indicators, event stream view, and theming.

**Answer:** Full mouse manipulation in v1. The first release includes the
core chrome (workspace sidebar, tab bar, pane titles/borders with agent
state and attention, click-to-focus, splits, theming) plus the full mouse
story: drag-to-rearrange tabs and panes, mouse border resizing, and
right-click context menus. Considered and rejected: deferring the drag/resize
/context-menu work to a fast-follow, and a bare-minimum tab-bar-only v1.

## Question 4

How does the workspace UI fit into the existing product surface? What
command launches it, and what happens to the existing stream TUI in
`crates/cyclops-ui` (launched today as `cyclops ui`)?

**Answer:** Bare `cyclops` opens the new workspace UI (TTY only — when stdout
is not a terminal it falls back to help so scripts never hang in an
unexpected TUI). The stream TUI moves from `cyclops ui` to `cyclops watch`:
plain `cyclops watch` opens the stream TUI, and `cyclops watch --json` keeps
today's machine-readable line-per-event stream unchanged so scripts and
agents don't break. `cyclops ui` is retired (optionally aliased during a
transition). This folds two overlapping commands — the old `watch` line
stream and the `ui` TUI with its `--plain` mode — into one surface.

## Question 5

While the workspace UI is attached, can a plain `tmux attach` client be
connected to the same session at the same time — and if so, who wins on
sizing?

**Answer:** Coexist, last-active wins — the same model Herdr uses (its server
sizes the shared layout to the most recently active "foreground client" and
falls back when that client detaches; see
`latest_active_client_drives_shared_size_theme_and_fallback` in Herdr's
server tests). For Cyclops this is tmux's built-in `window-size latest`
policy: the workspace UI declares its pane-canvas size via control mode,
plain clients stay attached, and whichever client is actively used sets pane
sizes. The workspace re-flows when another client changes them. Rejected:
pinning sizes to the workspace (clips smaller plain clients) and exclusive
attach (fights Cyclops's coexistence premise).

## Question 6

How is the keyboard divided between the workspace chrome and the focused
pane? With a composite renderer every keystroke lands in Cyclops first, and
agents in panes (vim, Claude Code, etc.) need most keys passed through
untouched — so what reserves keys for the chrome (sidebar, tabs, splits,
command actions)?

**Answer:** The Herdr-matching hybrid. Prefix-first defaults (`Ctrl+B` then a
key: next/previous tab, tab 1-9, new tab, workspace picker, pane swaps) so no
direct chord can collide with what an agent TUI or editor needs; every
binding is user-configurable to a direct chord (e.g. `ctrl+alt+]`) for power
users; the mouse is the primary chrome interaction, so the prefix is only for
keyboard-driven use. All other keys pass through to the focused pane
untouched. Herdr's default keymap is exactly this shape (prefix `ctrl+b`,
prefix-first and tab-centered defaults, rebindable to direct chords).

## Question 7

Which tmux sessions does the workspace show? Cyclops coexists with the
user's own tmux use — there may be sessions Cyclops never created (personal
sessions, other tools' sessions) on the same server. Does the sidebar list
every session on the tmux server, or only Cyclops-managed workspaces?

**Answer:** Show everything. Every session on the tmux server appears as a
workspace in the sidebar; sessions with detected agents get state and
attention decoration, plain sessions are ordinary usable terminals. This
matches how the daemon already behaves (it watches panes across the server
regardless of who created them) and avoids a session being visible to
`tmux attach` and `cyclops` commands but absent from the flagship UI.
Rejected: Cyclops-managed-only (hides real sessions) and opt-in adoption
(introduces view-membership bookkeeping with no backing in tmux or the
daemon — the second-source-of-truth problem ruled out in Question 2). Note:
Herdr has no analogue here; it owns every workspace it shows.

**Refinement — workspaces are projects:** A workspace is a project you are
working on, in the Herdr sense. "New workspace" means picking a project
folder: the UI creates a tmux session named after the folder with its
default directory set there, so every new tab and split opens in the
project. This is convention layered on plain tmux sessions (tmux's
session default-directory carries the semantics) — no new state, and
`tmux attach` sees an ordinary session. Foreign sessions made by hand still
appear as workspaces with whatever directory they happen to have.
Structurally Herdr's workspace → tabs → panes maps to tmux's session →
windows → panes; Herdr just adds the project anchoring, which we adopt.

## Question 8

How do agent attention and the event record surface inside the workspace?
The rough idea lists agent state/attention indicators and an event stream
view; `cyclops watch` remains the standalone stream TUI. What does the
workspace itself show when an agent needs a human, and how does the user
see recent activity without leaving the workspace?

**Answer:** Indicators plus a toggleable panel. The eye and per-agent state
badges live on sidebar rows, tabs, and pane borders (attention computed only
by `cyclops-proto`'s attention rule, never recomputed in the UI). A
slide-out event panel embeds the stream view for recent activity, toggled by
key or click, so the user never leaves the workspace to see the record.
Rejected for v1: indicators-only (forces opening `cyclops watch` in a pane
for any history) and toast notifications (transient, no record view).

## Question 9

Selection, copy, and scrollback interaction: with a composite renderer,
Cyclops owns these behaviors. What must v1 support when the user wants to
copy text out of a pane or scroll back through output?

**Answer:** Native feel. Mouse wheel scrolls the focused pane's history,
click-drag selects text within a pane, and selection copies to the system
clipboard. This matches the full-mouse v1 scope from Question 3 and Herdr's
behavior. (Because Cyclops draws the panes, the outer terminal's own
select-and-copy would grab the whole composed screen — chrome included — so
pane-local selection must be implemented by Cyclops.) Rejected: keyboard-only
copy mode in v1 and deferring to tmux copy-mode.
