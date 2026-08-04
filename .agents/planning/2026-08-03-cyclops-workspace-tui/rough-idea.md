````markdown
# Cyclops Terminal Workspace UI

## Status

Rough product and interaction idea for a new full-screen Cyclops interface.

## Overview

Build a polished terminal workspace for Cyclops that launches when the user runs:

```bash
cyclops
```

Cyclops should take over the user’s current terminal and open a full-screen TUI. It should not launch a separate desktop window.

The interface should combine:

- Herdr’s workspace organization and consistent terminal-native experience
- Cyclops’s existing tmux persistence, agent detection, pane naming, state tracking, messaging, and attention model

Tmux should continue to own the underlying sessions, windows, panes, PTYs, and running processes. `cyclopsd` should continue to own Cyclops state, agent detection, naming, messaging, themes, and persistence.

The result should feel like one calm workspace for coordinating coding agents rather than a collection of disconnected terminal panes.

---

## Core Principles

### Terminal-Native

Running `cyclops` should open the workspace inside the current terminal.

Detaching from the interface or closing the surrounding terminal should not terminate:

- The tmux session
- Running shell processes
- Coding agents
- `cyclopsd`

Running `cyclops` again should reconnect to the existing workspace.

### Terminals Remain Central

Terminal panes should occupy most of the screen.

Navigation, agent information, and controls should support the terminal workflow without turning Cyclops into a dashboard with terminals embedded inside it.

### Mouse-Accessible, Keyboard-Efficient

All common actions should be possible with the mouse, including:

- Switching workspaces
- Expanding workspace agent lists
- Switching tabs
- Selecting panes
- Resizing panes
- Splitting panes
- Rearranging panes
- Reordering tabs
- Opening menus
- Renaming panes
- Zooming panes
- Closing panes

The same actions should also have keyboard shortcuts.

### Minimal by Default

Cyclops should not require users to configure every pane before using it.

Unnamed panes should have no permanent chrome text. Naming a pane should progressively reveal the coordination features associated with a Cyclops identity.

### One Consistent Visual System

The workspace, sidebar, tabs, pane borders, menus, dialogs, existing CLI output, and `cyclops ui` should all use the same Cyclops theme system.

---

## Overall Layout

The interface should have three primary regions:

1. A workspace sidebar on the left
2. A tab bar across the top of the selected workspace
3. A pane canvas filling the remaining area

Example:

```text
┌────────────────┬──────────────────────────────────────────────┐
│ Workspaces     │ main     review     tests                 +  │
│                ├─────────────────────┬────────────────────────┤
│ ▾ cyclops    1 │                     │                  ▯▯  ▤ │
│   reviewer     │                     │                        │
│   Claude Code  │      terminal       │       terminal         │
│                │                     │                        │
│ ▸ website      ├─────────────────────┴────────────────────────┤
│ ▸ bucky        │                                              │
│                │                  terminal                    │
│                │                                              │
│ ☰ Menu         │                                              │
└────────────────┴──────────────────────────────────────────────┘
```

---

## Workspace Sidebar

Use a Herdr-style sidebar containing persistent workspaces.

Each workspace can be expanded to reveal the coding agents running inside it, regardless of whether those agents have been assigned Cyclops names.

Example:

```text
▾ cyclops
  reviewer       ● working
  Claude Code    ○ idle
  Codex          ⚠ needs input
▸ website
▸ bucky
```

### Agent Row Naming

Use this display priority:

1. The user-assigned Cyclops pane name, when present
2. The detected coding-agent name, such as Claude Code or Codex
3. A neutral fallback when Cyclops detects an agent but cannot identify it more specifically

A named pane should appear in the sidebar even when its current process is not recognized as a coding agent, because naming makes it an addressable Cyclops teammate.

An unnamed ordinary pane that is not recognized as an agent does not need to appear in the expanded sidebar.

An unnamed detected agent should still appear in the sidebar, even though it is not yet addressable through Cyclops messaging by name.

### Sidebar Behavior

The sidebar should support:

- Expanding and collapsing workspaces
- Clicking an agent to focus its pane
- Switching workspaces
- Showing attention indicators
- Highlighting the agent associated with the selected pane
- Reordering workspaces through dragging
- Hiding and showing the sidebar with a keyboard shortcut
- Preserving its width and collapsed state between sessions

The workspace list should scroll independently when needed.

There should not be a separate permanent agent-status panel in the bottom-left corner. The expandable workspace list should provide the workspace-level overview.

---

## Application Menu

Cyclops should have a persistent application-level menu button in the bottom-left corner of the workspace sidebar.

The button should remain fixed in the sidebar footer while the workspace list above it scrolls.

Example:

```text
│                │
│                │
│ ☰ Menu         │
└────────────────┘
```

Selecting the button should open an application-level menu upward from the bottom-left corner.

The initial menu should contain:

- Settings
- Keybindings
- Detach

Potential future items may include:

- Help
- About Cyclops
- Check for updates

### Application-Menu Behavior

- Clicking the menu button opens the menu
- Clicking the button again closes it
- Clicking anywhere outside the menu closes it
- Pressing `Escape` closes it
- Opening a pane context menu closes the application menu
- Opening the application menu closes any pane context menu
- Only one menu can be open at a time
- The menu should support mouse and keyboard navigation

### Detach

Selecting **Detach** should close the current Cyclops TUI and return the user to their shell without terminating:

- The tmux session
- Running processes
- Coding agents
- `cyclopsd`

Running `cyclops` again should reconnect to the previous workspace.

### Settings

Settings should edit the same Cyclops configuration used by the CLI rather than introducing a separate TUI-only configuration system.

Initial settings may include:

- Active theme
- Default workspace
- Sidebar visibility
- Sidebar width
- Mouse behavior
- Autosave behavior
- Agent and attention display preferences

### Keybindings

Keybindings should open a searchable or scrollable reference covering:

- Pane navigation
- Workspace navigation
- Tab navigation
- Split right
- Split down
- Rename pane
- Zoom pane
- Close pane
- Rearrange or move pane
- Open the application menu
- Open the pane context menu
- Detach

The first version may present a read-only reference. Customizable keybindings can be added later.

---

## Tabs

Tabs should appear across the top of the selected workspace.

A tab represents a tmux window or a group of panes within the workspace.

The intended mapping is:

```text
Cyclops workspace → tmux session
Cyclops tab       → tmux window
Cyclops pane      → tmux pane
```

Users should be able to:

- Create a tab
- Rename a tab
- Close a tab
- Reorder tabs through dragging
- Move panes between tabs
- Switch tabs using the mouse
- Switch tabs using keyboard shortcuts

Attention should roll up from panes into tabs. A tab containing an agent that needs input should show an indicator even when that tab is not selected.

### Tab Persistence

Tabs and their layouts should persist automatically.

When the user exits their terminal and later runs `cyclops` again, Cyclops should reopen:

- The previous workspace
- The previous active tab
- The same tabs
- The same pane layout
- The same running processes when the tmux session still exists

Structural changes should autosave, including:

- Creating, closing, or renaming a tab
- Reordering tabs
- Splitting or closing a pane
- Resizing panes
- Rearranging panes
- Moving panes between tabs
- Changing the active workspace or tab

Explicit workspace files can remain useful as reusable templates, but users should not need to manually run `cyclops workspace save` after ordinary interface changes.

---

## Pane Appearance

### Unnamed Panes

Unnamed panes should have no permanent chrome text.

They should not display the following in their border:

- Shell name
- Current directory
- Detected provider
- Agent state
- Pane number

Information about an unnamed pane may still appear in:

- The expanded workspace sidebar, when Cyclops detects an agent in it
- Search or command-palette results
- The pane context menu
- Temporary tooltips or inspectors

The pane canvas should remain visually quiet by default.

### Named Panes

The user can assign a Cyclops name through the pane’s right-click menu.

There is no separate role field.

The assigned pane name is also:

- The pane’s Cyclops identity
- Its message address
- Its sender identity
- Its roster label

A name such as `reviewer` may describe what the agent does, but Cyclops does not attach system prompts or separate role instructions to that value.

A named pane should show compact border chrome such as:

```text
reviewer · ● working
```

The border should show only:

- The assigned name
- The current agent state
- A stronger attention state when user intervention is required

Provider, directory, branch, task, and message history should not all be placed in the pane border.

### Selected Pane

The currently selected pane should have a clearly visible border using the Cyclops sage accent.

Inactive panes should use subtle neutral boundaries.

Mouse focus, keyboard focus, and the highlighted agent row in the sidebar should always agree.

Right-clicking a pane should also select it before opening its context menu.

### Pane Gaps

Use small gaps between panes.

The gaps should:

- Make pane boundaries easy to understand
- Give the selected-pane border room to appear
- Provide usable resize targets
- Preserve high terminal density

Panes should still feel like parts of one workspace rather than separate floating cards.

---

## Split Controls

Each pane should have exactly two contextual controls in its upper-right corner:

- Split right
- Split down

There should not be an overflow or menu button beside them.

The buttons should use visual icons that show the resulting layout:

- Two side-by-side rectangles for split right
- Two stacked rectangles for split down

The controls should appear when the pane is hovered or selected.

Example:

```text
┌────────────────────────────────────── ▯▯  ▤ ┐
│                                               │
│                   terminal                    │
│                                               │
└───────────────────────────────────────────────┘
```

Selecting a split action should:

- Split the current pane
- Create the new pane in the expected direction
- Focus the new pane
- Start it in the current pane’s working directory when possible
- Preserve all existing running processes

The same actions should remain available through right-click and keyboard shortcuts.

---

## Pane Context Menu

Right-clicking inside a pane should select that pane and open a contextual menu near the pointer.

The menu should contain only:

- Rename pane…
- Split right
- Split down
- Zoom pane
- Close pane

For a pane that already has a name, **Rename pane…** should allow the user to change or clear that name.

There should not be an **Assign role** action.

### Context-Menu Behavior

- Clicking an action performs it and closes the menu
- Clicking anywhere outside the menu closes it without taking an action
- Pressing `Escape` closes it
- Opening another pane context menu closes the current one
- Opening the application menu closes the pane context menu
- Only one menu can be open at a time
- Normal left-click terminal interaction should not open the menu
- Right-click text-selection behavior will need to be reconciled with the context menu

---

## Drag-to-Rearrange

Drag-to-rearrange should ship in the initial version.

Users should be able to:

- Reorder tabs
- Rearrange panes within a tab
- Move a pane to another tab
- Move a pane to another workspace where supported
- Choose the destination split position visually

During a pane drag, Cyclops should show clear drop zones indicating where the pane will land.

Example:

```text
┌─────────────┬─────────────┐
│             │             │
│    left     │    right    │
│             │             │
├─────────────┴─────────────┤
│          bottom           │
└───────────────────────────┘
```

Dragging should begin from a deliberate drag target or pane edge so selecting terminal text does not accidentally move the pane.

Because tmux owns the real layout, every completed drag must be translated into valid tmux operations and reconciled against the resulting geometry.

The saved workspace layout model may need to evolve beyond its current row-based representation to support arbitrary pane arrangements cleanly.

---

## Resizing

Users should be able to resize panes by dragging the gaps or dividers between them.

Resize targets should be easy to discover without making the dividers visually heavy.

The interface should:

- Update responsively while dragging
- Avoid flicker
- Keep terminal dimensions synchronized with tmux
- Preserve the updated proportions automatically
- Maintain a usable minimum pane size

---

## Agent State and Attention

Named panes may show states such as:

- Working
- Idle
- Waiting
- Blocked
- Needs input
- Dead

State should always use a combination of:

- Text
- Glyph or shape
- Color

Cyclops should not rely on color alone.

### Attention Hierarchy

Attention should roll upward:

```text
pane → tab → workspace
```

When an agent requires intervention:

- Its pane border should change
- Its sidebar row should receive an indicator
- Its tab should receive an indicator
- Its workspace should receive an indicator
- Clicking the indicator should jump to the relevant pane

Routine working and idle transitions should remain visually quiet.

---

## Detailed Event Stream

The primary `cyclops` workspace should not contain a permanent event feed, message-history panel, or administrative firehose.

Detailed events should remain available through:

```bash
cyclops ui
```

Users who want to inspect state transitions, messages, delivery receipts, or the full event stream can explicitly run `cyclops ui` in another terminal or pane.

The main workspace should show only immediate, actionable information.

---

## Themes

The entire interface should use the existing Cyclops theme system.

The default Cyclops visual identity should include:

- Cyclops dark theme as the default
- Cyclops light theme as the main alternative
- Sage accents for selection and branded emphasis
- Existing semantic agent-state and attention colors
- Subtle neutral separators and inactive borders

Theme selection should remain consistent with:

```bash
cyclops theme <name>
```

A theme switch should apply consistently to:

- Workspace sidebar
- Tab bar
- Pane borders
- Selected pane
- Split controls
- Application menu
- Pane context menu
- Settings
- Keybindings
- Dialogs
- Attention indicators
- Existing CLI output
- `cyclops ui`
- Existing tmux border chrome

The workspace UI may require additional semantic theme tokens for:

- Sidebar surfaces
- Active and inactive tabs
- Hovered rows
- Selected rows
- Active and inactive pane borders
- Menu surfaces
- Drag targets
- Modal surfaces

These should extend the existing theme engine rather than creating a separate UI theme configuration.

No TUI component should hardcode its own colors.

---

## Visual Fluidity

Because Cyclops remains a TUI, fluidity should mean:

- Immediate response to mouse and keyboard input
- Stable layouts
- Minimal flicker
- Responsive pane resizing
- Clear spatial continuity during rearrangement
- Consistent visual styling
- Avoiding unnecessary full-screen redraw artifacts
- Keeping terminal input responsive during UI updates

---

## Persistence and Reopening

Running `cyclops` should reopen the user’s last active workspace and tab.

Fallback order:

1. Last active workspace and tab
2. Configured default workspace
3. First available workspace
4. A new default workspace

Live tmux sessions should remain the source of truth for running processes.

Cyclops should also autosave enough structural state to reconstruct:

- Workspace names
- Tabs
- Pane layouts
- Pane names
- Working directories
- Selected workspace
- Selected tab
- Sidebar state

Restoring structure after a tmux session has disappeared does not necessarily imply restoring the exact running processes.

---

## Initial Scope

The first version should include:

1. `cyclops` launches the full-screen workspace
2. Persistent workspace sidebar
3. Expandable agent lists under workspaces
4. Bottom-left application menu
5. Settings, Keybindings, and Detach actions
6. Persistent top tabs
7. Live interaction with existing tmux panes
8. Automatic reopening of the previous workspace and tab
9. Small pane gaps
10. Clear selected-pane border
11. Exactly two visible split controls
12. Right-click pane context menu
13. Click-outside and `Escape` dismissal for all menus
14. Mutual exclusivity between application and pane menus
15. Mouse support for all common actions
16. Keyboard equivalents for all common actions
17. Rename and clear-name behavior
18. No chrome text for unnamed panes
19. Compact identity and state chrome for named panes
20. Drag-to-rearrange panes and tabs
21. Mouse-driven pane resizing
22. Pane, tab, and workspace attention indicators
23. Automatic structural persistence
24. One consistent, switchable Cyclops theme system
25. Detaching without terminating agents or tmux sessions

---

## Remaining Implementation Decisions

- What fallback label should appear for an unnamed detected agent in the sidebar?
- Should the two split controls appear on hover, selection, or both?
- What exact keyboard shortcuts avoid conflicts with tmux, shells, editors, and coding-agent CLIs?
- How should right-click terminal text selection coexist with the pane context menu?
- What drag handle provides discoverability without adding excessive pane chrome?
- Does the current row-based workspace format need to become an arbitrary split tree?
- Which additional semantic tokens should be added to `cyclops-theme`?

The design bias should be toward:

- Minimal permanent UI
- Progressive disclosure
- Maximum space for terminal panes
- Clear mouse interactions
- Fast keyboard workflows
- Consistency across every Cyclops surface
````
