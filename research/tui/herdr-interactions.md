# Herdr TUI interaction system

## Scope and provenance

This is a source-only study of Herdr's TUI interaction system. It covers input acquisition, keymaps, mouse behavior, focus and navigation, workspaces/tabs/panes, scrollback and copy mode, dialogs and notifications, agent-status interaction, PTY routing, clipboard and paste, resize, detach/shutdown, concurrency, usability, and tests. It intentionally does not compare Herdr with Cyclops or propose an adoption plan.

- Repository: <https://github.com/herdrdev/herdr>
- Inspected commit: [`2863b715132fe29e53089e06f105943d1df0b3b4`](https://github.com/herdrdev/herdr/tree/2863b715132fe29e53089e06f105943d1df0b3b4)
- Commit timestamp: 2026-08-05T19:34:24+03:00
- Commit subject: `feat(windows): support remote attach to unix hosts (#2329)`
- Inspection date: 2026-08-05
- Evidence basis: upstream source, upstream tests, upstream contributor instructions, and upstream documentation at that commit. No live Herdr session was controlled or manually exercised.

`Evidence` below means the statement is directly supported by the linked source. `Inference` means it follows from several source facts but was not stated as a contract by Herdr's maintainers. Negative findings are explicitly qualified because source search is not a runtime probe.

## Executive summary

1. **Herdr is mouse-first but not mouse-dependent.** Its contributor rules call it a mouse-first TUI, while its active keymap is configurable, searchable in-product, and broad enough to operate workspaces, tabs, panes, settings, and copy mode without a mouse. The default prefix is `Ctrl+B`; direct bindings are also supported. [Evidence: project principles](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/AGENTS.md#L26-L47), [keyboard guide](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/docs/next/website/src/content/docs/keyboard.mdx#L10-L67).

2. **Interaction is a client/server protocol, not a local Crossterm loop.** A thin terminal client owns raw-mode setup, input framing, host resize detection, frame output, clipboard forwarding, and terminal cleanup. A server owns shared session state, parses/routes input, writes PTYs, computes per-client views, and streams semantic or ANSI frames. [Evidence: client architecture](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1269-L1398), [server events](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/client_transport.rs#L281-L355).

3. **A finite `Mode` enum is the top-level interaction state machine.** Terminal, prefix, navigation, copy, resize, rename, confirmation, context menu, settings, help, navigator, onboarding, announcements, and worktree flows are explicit modes. A popup terminal is a higher-priority capture layer outside that enum. [Evidence: modes](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/state.rs#L817-L870), [key dispatcher](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mod.rs#L77-L124).

4. **Keyboard routing preserves press/repeat/release ownership.** Once a press is forwarded to a terminal, later repeats and the release remain targeted to that terminal even if focus or mode changes. Focus loss releases all keys held by that input source. This addresses stuck keys and cross-pane repeat leakage in a multi-client, enhanced-keyboard-protocol environment. [Evidence: runtime raw-input handling](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/runtime.rs#L94-L267), [headless repeat routing](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/mod.rs#L1594-L1649).

5. **Mouse hit-testing consumes the geometry produced before render.** `compute_view` mutates normalized layout state and stores pane, split, tab, sidebar, mobile, and toast hit areas in `ViewState`; `render` only reads that state. Mouse handlers consume the same geometry. [Evidence: view state](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/state.rs#L792-L815), [compute phase](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui.rs#L108-L156), [desktop geometry](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui.rs#L215-L324), [pure render](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui.rs#L389-L462).

6. **PTY-aware arbitration is pervasive.** Keyboard encoding follows the child pane's negotiated keyboard protocol. Mouse wheel input chooses among application mouse reporting, alternate-scroll key synthesis, and host scrollback. Paste is bracketed only when the child requested it. Plain PageUp/PageDown are intercepted only for a shell-like primary-screen state; unknown state fails open to the child. [Evidence: terminal key forwarding](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/terminal.rs#L63-L235), [pane input state](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/pane/terminal.rs#L113-L139), [paste payload](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/pane.rs#L2694-L2730), [wheel forwarding](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L1609-L1775).

7. **The rendering transport is deliberately lossy for stale visual work but ordered for control messages.** Each client writer has an ordered control queue and only one render slot. A full render slot causes the server to mark a deferred latest frame rather than blocking interaction. Per-client render baselines suppress identical frames and support semantic or terminal-ANSI encodings. [Evidence: writer queue](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/client_transport.rs#L183-L266), [render baselines](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/render_stream.rs#L12-L156), [deferred send](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L4283-L4324).

8. **Interaction behavior has unusually dense tests.** Tests cover gesture precedence and hitbox boundaries, pane mouse-protocol forwarding, selection autoscroll, copy-mode focus/resize/search semantics, key repeat leases, multi-client view sizing and promotion, detach/reattach, graceful cleanup, and terminal-protocol corpora. Selected coverage is mapped below.

## Input-to-effect flow

```text
host terminal
  │  raw bytes on Unix; VT/Win32 semantic records on Windows
  ▼
thin client input thread
  ├─ frames ambiguous Esc, mouse, paste, focus, query responses
  ├─ bridges remote clipboard images / image file drops
  └─ sends through bounded client-loop channel
  ▼
client socket protocol
  ├─ Input { raw bytes }
  ├─ InputEvents { semantic events }
  ├─ Resize / Detach / ClipboardImage
  └─ size and payload validation
  ▼
server client event loop
  ├─ tracks per-client focus, size, theme, keymap, activity
  ├─ promotes an interacting full-app client to foreground
  └─ parses raw input to RawInputEvent
  ▼
App raw-input router (tagged by input-source/client ID)
  ├─ input lease: press → repeat plan → release to stable terminal ID
  ├─ popup capture
  ├─ Mode dispatch: terminal / prefix / copy / modal / navigator / …
  └─ mouse arbitration: overlay → Herdr chrome → pane protocol → scrollback
  ▼
effect
  ├─ pure/session state mutation and API-style action helper
  ├─ PTY bytes, bracketed paste, focus event, or mouse report
  ├─ clipboard / notification / detach control message
  └─ render-dirty request
  ▼
compute_view(state, client area) → render(&state) into virtual Ratatui buffer
  ▼
per-client semantic frame or ANSI diff → one-slot render queue
  ▼
thin client blit/commit → host terminal
```

The main evidence for this sequence is the client's three producer threads and bounded channel, the server event conversion and foreground promotion, App's raw-event lease handling, and the compute/render split. [Client producers](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1313-L1398), [server routing](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L2678-L2745), [App dispatch](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mod.rs#L77-L208), [render loop](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/mod.rs#L1040-L1116).

## Interaction inventory

| Surface | Keyboard interaction | Mouse interaction | State/effect |
| --- | --- | --- | --- |
| Global command entry | Prefix, default `Ctrl+B`; direct bindings may bypass it | Bottom-right launcher menu | Enters `Prefix` or `GlobalMenu` |
| Workspace | Create, rename, close, previous/next, indexed 1–9, picker | Card click, scrollbar, wheel, press-drag reorder, right-click menu, group expand/collapse | Changes selected/active workspace; requests dialogs or API-style mutations |
| Tab | Create, rename, close, previous/next, indexed 1–9 | Click, overflow arrows, new-tab button, wheel cycle, press-drag reorder, right-click menu | Changes active tab and focus; may prompt for a name |
| Pane | Directional focus/swap, cycle, last pane, split, close, zoom, resize, rename | Click focus, divider drag, scrollbar drag/track click, right-click menu | Changes layout/focus or routes to pane runtime |
| Terminal child | Unclaimed keys/text/paste; negotiated enhanced keyboard encoding | Mouse reports when requested; otherwise selection and host scrollback | Writes bytes to a stable terminal ID |
| Copy/scrollback | Vi/tmux-like movement, search, select, yank, editor action | Drag selection, double-click word, wheel during selection, pane scrollbar | Maintains a pane-pinned copy state and terminal scroll offset |
| Navigator | Filter keys, search `/`, arrows/j/k, Enter, Space, page moves | Hover-select, row click, caret toggle, wheel; outside click dismisses | Filters/focuses agent/workspace/tab targets |
| Settings/help | Arrows/j/k, Enter, search, Esc/back | Tabs, rows, buttons, scrollbar, hover | Mutates settings or returns through modal stack |
| Notifications | Configurable “open notification target” action | Click targeted agent toast | Focuses workspace/tab/pane and clears toast |
| Popup terminal | All keys/text/paste go to popup | Pane-local mouse/wheel routing | Popup has priority over normal mode handling |
| Detach | Default prefix action, direct-attach `Ctrl+B q`, Ctrl+C client exit path | Global-menu action | Removes only client; server/session remains unless configured otherwise |
| Responsive/mobile | Same command model; navigation mode opens switcher | Header/menu and single-column switcher hit areas | `ViewLayout::Mobile` selected below configurable width threshold |

The source action inventory is larger than the default keyboard guide: it also includes worktree operations, agent next/previous/indexed focus, last-pane, notification target, settings, reload, navigator, custom commands, and scrollback editor. [Action dispatch](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/navigate.rs#L183-L423), [keybind help is built from the effective keymap](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui/keybind_help.rs#L62-L181).

## Input acquisition and terminal ownership

### Terminal setup and cleanup

`Evidence:` The thin client calls `ratatui::init`, clears stale host mouse reporting, optionally enables mouse capture, enables bracketed paste and focus-change reporting, pushes keyboard enhancement flags, and disables line wrapping. A `TerminalGuard` restores modifyOtherKeys, keyboard flags, line wrapping, focus/paste/mouse modes, Ratatui terminal state, the cursor, and Windows input modes on drop. A panic hook invokes the same restoration, and the `ctrlc` termination feature funnels SIGINT/SIGTERM/SIGHUP through the normal quit path. [Setup](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L332-L417), [restore and guard](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L566-L653), [termination handler](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1200-L1227).

`Evidence:` Terminal setup occurs after a successful server handshake, avoiding raw-mode leakage when attach is rejected. Client tests attach under a PTY and assert mouse teardown bytes after server EOF and SIGHUP. [Setup ordering](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1180-L1210), [cleanup tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/tests/client_mode.rs#L711-L772).

### Unix and Windows input

`Evidence:` Unix reads locked stdin into 4 KiB buffers, runs a `RawInputByteFramer`, coalesces host query responses, and flushes pending ambiguous input on timeouts. Windows selects either VT-input or Crossterm/Win32 translation; the Crossterm path polls at 10 ms. The semantic conversion preserves text commits, key press/repeat/release, generated text, physical/VT source data, mouse, paste, and outer focus. [Input reader](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/input.rs#L1-L184), [semantic conversion](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/input.rs#L385-L437).

`Evidence:` Stdin, resize, and server-reader threads feed a bounded Tokio channel of 256 events. The async client loop selects the channel plus a 100 ms wakeup, while socket writes occur from that loop. [Client loop](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1269-L1398).

`Inference:` The design serializes host input, server frames, resize, and control changes at the client-loop boundary, while keeping blocking terminal and socket reads off the async loop. The 100 ms timer is also a quit-progress wakeup; source alone does not show a stronger latency guarantee.

### Transport limits

`Evidence:` The server expands repeat counts when checking event-batch limits and counts generated text, raw VT bytes, text commits, and paste bytes. An oversized complete paste is rejected with an in-app notification; oversized non-paste input and too many events disconnect the client. [Limit calculation](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/client_transport.rs#L381-L443), [enforcement](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/client_transport.rs#L701-L769), [rejection toast](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L2919-L2934).

## Keyboard model, keymaps, and command dispatch

### Binding model

`Evidence:` Each resolved binding is either `Direct(KeyCombo)` or `Prefix(KeyCombo)`. Actions accept multiple bindings; indexed bindings expand 1–9 and retain compatibility with legacy shifted-digit encodings. Custom bindings can launch detached shell commands, pane commands, popup terminal commands, or plugin actions. [Binding types](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/config/keybinds.rs#L120-L290).

`Evidence:` Direct and prefix namespaces are registered separately. The prefix itself is reserved. Conflicting bindings are diagnosed and disabled, and an unmodified printable direct binding is rejected because it would intercept typing. [Registry](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/config/keybinds.rs#L369-L454), [validation](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/config/keybinds.rs#L1000-L1041).

`Evidence:` In terminal mode, precedence is retained-selection copy, selection clearing, direct built-in action, direct custom command, direct indexed action, prefix activation, modifier-only suppression, and finally PTY forwarding. Pressing the prefix twice sends the literal prefix to the pane; Escape leaves prefix mode. [Terminal precedence](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/terminal.rs#L63-L138), [prefix handler](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/navigate.rs#L60-L124).

### Action handling

`Evidence:` The production action path groups workspace, tab, pane, UI, agent, and session commands under `NavigateAction`. Prefix actions usually execute and return to terminal mode; navigation mode reserves arrows, Tab/BackTab, Enter, workspace digits, and configured vi-style keys. Indexed action priority is exact tab binding, then workspace, then agent. [Navigate mode](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/navigate.rs#L127-L181), [reserved and indexed dispatch](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/navigate.rs#L1246-L1446).

`Evidence:` Custom command failures become non-targeted attention toasts. Detached shell commands receive Herdr socket and active workspace/tab/pane/cwd environment variables. Pane and popup actions create terminal runtimes; plugin actions invoke a plugin command. [Custom commands](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/navigate.rs#L785-L894).

### Key protocol and routing leases

`Evidence:` `TerminalKey` carries semantic code/modifiers/kind, repeat count, shifted codepoint, generated text, and source metadata. Encoding favors generated text for non-release events, only emits releases when the negotiated Kitty protocol reports event types, and otherwise chooses Kitty CSI-u or legacy encoding. [Key model](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/input/model.rs#L1-L160), [encoder](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/input/encode.rs#L14-L75).

`Evidence:` A forwarded key target contains a stable terminal ID rather than a current pane index. Press handling records whether a key was forwarded, consumed, or should be reprocessed; repeats either remain on the forwarded target, repeat a still-valid command context, or are ignored. Release goes to the recorded target, and input-source removal synthesizes releases. [Target preparation](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/terminal.rs#L138-L235), [lease runtime](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/runtime.rs#L94-L267), [source release](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/terminal.rs#L319-L404).

`Evidence:` Herdr asks the host for Kitty “report all keys” while in prefix/navigation modes, or when the popup/focused pane requires compatible reporting. The server tells only the active client to toggle this terminal mode. [Host protocol request](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/terminal.rs#L267-L292), [client toggle](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1597-L1617).

## Mouse hit-testing and gestures

### Precedence

`Evidence:` Top-level mouse precedence is onboarding, targeted toast, settings, launcher, global menu, keybind help, mobile UI, right-click passthrough, worktree/confirmation/name/context overlays, then chrome and panes. The App wrapper adds popup capture, URL-click release suppression, modified URL opening, and double-click word selection before applying returned semantic `MouseAction`s. [State-level precedence](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L101-L220), [App-level dispatch](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mod.rs#L329-L477).

`Evidence:` When Herdr's mouse capture setting is off, normal TUI chrome does not receive mouse events; events inside terminal panes are still forwarded when the child requested mouse reporting. The host enables capture if the global setting is on, a popup is open, or the focused child requests it. [No-capture routing](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L72-L99), [capture predicate](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/state.rs#L1655-L1674).

### Click, drag, reorder, and resize

`Evidence:` A left press clears stale selection/press state, then may begin sidebar-divider, sidebar-section, split-divider, scrollbar, tab, workspace, or terminal selection interaction. Split dragging records the layout path, split direction, source area, and grab offset, then clamps ratios. Workspace and tab presses become reorder drags only after a movement threshold; otherwise release performs focus. [Press and drag setup](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L223-L660), [drag/release](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L664-L909).

`Evidence:` Pane split hit-testing distinguishes borders, gaps, frames, and inner content. Tests specifically guard one-cell/zero-width boundaries and ensure a split hitbox does not steal the adjacent pane's first content cell. [Hit-testing helpers](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L1388-L1460), [boundary tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L2993-L3228).

`Evidence:` Right-click normally opens a context menu. An exact configured modifier can instead pass the complete down/drag/up gesture to a mouse-reporting pane; Herdr strips its own passthrough modifier and locks the gesture to the original pane until release. [Passthrough](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L1533-L1596).

### Wheel routing

`Evidence:` Wheel over the tab bar cycles tabs. Wheel in an active selection scrolls and extends that selection. Wheel over a pane first focuses the pointed pane, then uses `WheelRouting`: application mouse report, alternate-scroll encoding, or Herdr scrollback. Horizontal wheel is forwarded only to a child requesting mouse reports. Sidebar wheel separately targets the workspace list or agent panel. [Wheel branches](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L919-L1017), [pane wheel routing](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L1609-L1775).

### URLs and double-click

`Evidence:` A platform-specific modified left click in terminal mode resolves OSC 8 metadata or visible URL text at the pane cell, offers it to a plugin link handler, then opens it through the platform. The input-source ID records the pending URL click so its later release is not accidentally delivered to a pane application. [URL click](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mod.rs#L562-L599).

`Evidence:` Double-click selection requires an unmodified terminal-pane click near the previous cell, in the same pane and within the gesture window. Any drag or completed selection invalidates the candidate. Selected words honor display columns and optionally copy immediately. [Double-click state machine](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mod.rs#L601-L691).

## Focus, workspaces, tabs, panes, and multi-client control

`Evidence:` Pane focus is represented by stable workspace ID plus pane ID. Focusing a pane may switch workspace and tab, focuses the layout node, records previous focus, marks the session dirty, and synchronizes copy mode. [Focus mutation](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/actions.rs#L270-L345).

`Evidence:` Multiple attached clients can receive frames, but the last full-app client that interacts becomes foreground. Interaction includes key, text, mouse, paste, and focus-gained, but not focus-lost. Foreground state determines shared terminal size, outer focus, cell size, host theme, and effective keybindings. If it disconnects, the latest remaining app client is promoted. [Interaction detection](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/clients.rs#L237-L274), [promotion and shared state](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L1085-L1137), [routing/promotion](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L2678-L2735).

`Evidence:` A non-foreground client gets a view computed at its own area without resizing shared pane runtimes. Foreground size changes recompute geometry and resize active/background panes, then request repaint for every client. [Non-foreground compute contract](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui.rs#L139-L156), [shared resize](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L1026-L1081).

`Inference:` “Foreground” is an interaction lease over a shared view/runtime, not an exclusive connection. Background clients remain viewers and can become foreground through interaction. The source tests exercise both shared broadcast and promotion-sensitive sizing.

## Selection, scrollback, copy mode, and editor handoff

### Mouse selection

`Evidence:` Drag selection is pane-pinned. At or beyond the top/bottom edge, a 30 ms scheduled autoscroll state continues moving and extending selection even if the mouse stops. Speed scales with distance beyond the pane and is clamped to 3–15 lines. The edge hot zone activates only after a real drag, not a click. [Autoscroll state](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/state.rs#L66-L89), [selection behavior](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/selection.rs#L9-L168), [deadline handling](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/runtime.rs#L335-L352).

`Evidence:` With copy-on-select enabled, release copies and clears on a short feedback schedule. With it disabled, finalized selection remains, and exact Ctrl+C or Super+C copies it instead of forwarding the key. A consumed input lease suppresses repeated copy shortcuts. [Retained copy](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/clipboard.rs#L10-L56), [clipboard tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/clipboard.rs#L132-L184).

### Copy mode

`Evidence:` Entering copy mode captures the focused pane ID, current cursor or bottom-row fallback, entry scroll offset, and current geometry. It clears mouse selection and sets `Mode::Copy`. [Entry](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/copy_mode.rs#L29-L76).

`Evidence:` Commands include arrows and h/j/k/l, PageUp/PageDown, Ctrl+B/F/U/D, g/G, 0/$/^, w/b/e, paragraph motion, forward/backward literal search, n/N repeat, character or linewise selection, and yank/Enter. Escape first clears active selection/search and only exits when there is nothing to clear. The prefix retains priority over copy-mode commands. [Commands](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/copy_mode.rs#L13-L241).

`Evidence:` The underlying process remains live. At the bottom, copy mode follows output; in history, it remains pinned. Focus away clears only the visible copy selection and returns to terminal mode while preserving the pane's copy-mode state and scroll position; focusing the source pane restores `Mode::Copy`. Pane removal clears the state. Geometry changes clear stale search matches but retain the query. [Focus and geometry synchronization](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/copy_mode.rs#L750-L831), [documented behavior](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/docs/next/website/src/content/docs/keyboard.mdx#L65-L67).

### External editor

`Evidence:` The default `prefix+e` action snapshots all recent focused-pane text into a temporary file, resolves a platform-specific `$EDITOR` command, and opens it as an overlay pane. Errors become attention toasts; temp files are cleaned on launch failure and assigned to the spawned pane for cleanup. [Editor handoff](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/navigate.rs#L896-L960).

## Prompts, overlays, popup terminals, and notifications

`Evidence:` Modal keyboard behavior is defined around reusable Enter, Escape, and Ctrl+C action specs, and mouse buttons use shared rectangles. The global menu exposes Settings, Keybinds, Reload, optional What's New, and Detach; arrows/j/k move and Enter activates. [Modal primitives and global menu](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/modal.rs#L17-L160).

`Evidence:` Release notes, product announcements, navigator, and keybind help own their mouse input while open, including custom scrollbars. Navigator hover moves selection; click activates a row or toggles workspace expansion; click outside dismisses it. [Overlay mouse handling](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/overlays.rs#L21-L190).

`Evidence:` A popup terminal has priority over all normal modal/terminal dispatch. Escape is encoded and sent to the popup rather than closing it. Text and bracketed paste also go to the popup runtime. [Popup key routing](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/terminal.rs#L238-L265), [text/paste routing](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mod.rs#L126-L208).

`Evidence:` Agent-state changes create delayed or immediate deliveries only after revalidating the pane's expected state and agent identity. Background-tab state can produce an in-app toast, client/system notification, and sound. A targeted toast carries stable workspace/pane identity and is clickable to focus that pane; overlays take precedence so a toast cannot steal a settings click. [Delivery state](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/actions.rs#L3069-L3234), [toast target](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/state.rs#L1326-L1357), [toast mouse priority](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L100-L135).

## Agent-status interaction

`Evidence:` A completion transition to idle marks a pane unseen when it is not suppressed by an active, focused foreground view; non-idle updates mark it seen. Focusing the outer terminal marks the active tab seen. Thus the visual “done” state is `Idle + unseen`, distinct from ordinary seen idle. [Seen transition](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/actions.rs#L3069-L3095), [foreground focus synchronization](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L1085-L1131).

`Evidence:` Workspace/agent attention ordering is blocked, done, working, idle, unknown. The sidebar, navigator, mobile view, and API expose status labels and stable focus targets. A setting can use color dots or distinct symbols (`×`, `◐`, `✓`, `○`, `·`), while text labels map to blocked/working/done/idle. [Aggregation](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/workspace/aggregate.rs#L75-L100), [symbols and labels](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui/status.rs#L196-L234), [settings choice](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui/settings.rs#L105-L123).

`Evidence:` Navigator can filter agents to blocked, working, idle, or done, search by text, move by rows or half-pages, expand workspaces, and accept a target. [Navigator keyboard handling](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/modal.rs#L162-L260).

## PTY input, clipboard, and paste

`Evidence:` Text commits are not treated as commands: in terminal mode they clear selection and write UTF-8 directly to the focused runtime; in a text-entry modal they insert into that modal; in a popup they go to the popup. Paste takes the same target hierarchy but calls `send_paste`, which consults the child input state and adds `ESC[200~`/`ESC[201~` only when bracketed paste is enabled. [Text and paste dispatch](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mod.rs#L126-L230), [paste encoding](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/pane.rs#L2699-L2730).

`Evidence:` Clipboard writes are App events. In headless mode the server sends base64 clipboard data only to the foreground client; the client decodes it and emits OSC 52 locally. This keeps the server away from the attaching machine's clipboard. [Clipboard request](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/clipboard.rs#L15-L28), [server forwarding](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L2090-L2116), [client forwarding](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L2048-L2066).

`Evidence:` For remote sessions, the client can intercept a configured image-paste chord or an image file drop, read a bounded local image, send it to the server, stage it, and route the resulting remote path through normal paste handling. Observe-only clients cannot paste. [Client bridge](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1453-L1495), [drop parsing and size bound](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1940-L2000), [server staging/routing](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L1676-L1716).

## Resize, detach, shutdown, suspend, and resume

### Resize

`Evidence:` The client installs a platform resize signal but also checks geometry every 100 ms. It reports both changed geometry and a signal that returns the same size, including pixel cell size. The server stores every client's size; a full-app resize promotes that client to foreground and recomputes/resizes the shared runtime. Direct terminal attachments resize only their target runtime; observers repaint without taking control. [Resize watcher](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L2218-L2277), [server resize routing](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L2961-L3021).

### Detach and shutdown

`Evidence:` In persistent server mode, the detach action sets `detach_requested`; the server sends that client `ServerShutdown { reason: "detached" }`, cleans its graphics, removes it, and leaves the session running. Direct attach reserves `Ctrl+B q` for detach. On ordinary client Ctrl+C/termination, the client sends `ClientMessage::Detach` before returning. [Detach request](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/modal.rs#L123-L145), [server detach response](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L2737-L2745), [direct-attach escape](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L103-L159), [client clean exit](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1628-L1645).

`Evidence:` Server SIGINT initiates a shutdown broadcast before closing connections. Tests verify the message, terminal restoration after EOF/SIGHUP, session survival across detach, output accumulation while detached, and PTY size persistence. [Graceful shutdown test](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/tests/client_mode.rs#L894-L953), [detach tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/tests/detach_reattach.rs#L256-L420), [detached output tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/tests/detach_reattach.rs#L641-L738).

### Suspend/resume uncertainty

`Negative finding:` I found no explicit SIGTSTP/SIGCONT or Ctrl+Z suspend/resume terminal-mode choreography in the inspected `src/client`, `src/server`, `src/app`, `src/main.rs`, and `src/platform` paths. “Agent resume” in the repository refers to restoring agent sessions, not suspending the TUI process. Source inspection alone cannot establish how a shell/job-control suspend behaves, so this remains unverified rather than a claim of missing runtime support.

## Accessibility and usability observations

`Evidence:` Herdr offers both mouse discovery and full keyboard control. Effective bindings, including custom commands, are generated into searchable in-product help rather than copied from defaults. [Mouse-first rule](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/AGENTS.md#L30-L37), [dynamic help](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui/keybind_help.rs#L19-L181).

`Evidence:` A mobile single-column layout activates below a configurable terminal width. It has dedicated header and switcher hit areas rather than compressing desktop hitboxes. [Mobile computation](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui.rs#L326-L387), [configuration](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/config/model.rs#L809-L857).

`Evidence:` Status can use distinct shapes instead of color-only dots. Panels choose a contrast foreground, and agent rows also have textual state labels. [Indicator setting](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui/settings.rs#L105-L123), [symbols/labels](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui/status.rs#L196-L234).

`Evidence:` CJK/IME accommodations include generated-text preservation, mode-level ASCII input-source switching for command modes on supported platforms, and configurable native/drawn host cursors. The source explicitly records a limitation: Navigator and KeybindHelp search remain forced to ASCII because the predicate operates at whole-mode granularity. [IME mode predicate and limitation](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/state.rs#L841-L870), [host cursor configuration](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/config/model.rs#L133-L140).

`Uncertainty:` I found no screen-reader-specific semantic output or accessibility-tree mechanism. That is not surprising for a terminal grid, but no live assistive-technology testing was performed.

## Concurrency and message-passing findings

- `Evidence:` Client-side blocking producers are isolated in stdin, resize, and server-reader threads; a bounded channel serializes their events. [Client concurrency](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1313-L1398).
- `Evidence:` Server transport threads emit typed `ServerEvent`s into the main event loop. App API, internal runtime events, input, render notification, and nearest scheduled deadline are selected in one async loop. [Server events](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/client_transport.rs#L281-L355), [App loop](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/mod.rs#L1098-L1130).
- `Evidence:` Raw input is batch-drained before other work, but geometry-dependent deferred agent-resume work is withheld until after a render refreshes pane geometry. [Raw-input batch](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/runtime.rs#L94-L112), [scheduled geometry guard](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/runtime.rs#L379-L385).
- `Evidence:` Scheduled work is represented as nearest deadlines for resize, toast, notification, selection autoscroll, copy feedback, session save, metadata expiry, and render throttling. [Deadline aggregation](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/runtime.rs#L570-L608).
- `Evidence:` Control messages are queued in order; only one render may be in flight. A full render slot records deferred work, so the later frame is regenerated against current state when the writer drains. [Queue](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/client_transport.rs#L183-L266), [defer behavior](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L4283-L4317).
- `Inference:` Interaction state mutations are effectively serialized through the server/App loop, while PTY IO, terminal reads, and frame writes are concurrent. Stable IDs and input-source leases are what make that boundary safe when focus changes during asynchronous activity.

## Interaction tests

The following is a representative map, not an exhaustive list.

| Test layer | Behaviors locked down | Primary source |
| --- | --- | --- |
| Raw key protocol corpus | 39-line shared fixture parsed both as terminal keys and whole raw events; terminal-specific variant fixtures | [parser test](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/input/parse.rs#L890-L945), [raw extractor test](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/raw_input.rs#L1840-L1874) |
| Copy mode | Prefix precedence, focus-away preservation, source tab removal, search wrap, resize invalidation, linewise selection, page sizing, cancel/restore offset, enhanced shifted punctuation | [copy-mode tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/copy_mode.rs#L1159-L2042) |
| Terminal/key leases | Direct action interception, popup ownership, PageUp arbitration, grouped repeats, context changes, release suppression, missing runtime behavior | [terminal tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/terminal.rs#L1318-L2073) |
| Mouse and hitboxes | Wheel routing, right-click passthrough, focus-before-forward, pixel-motion downgrade, toast precedence, context menus, split-boundary cells, reorder, mobile interactions | [mouse tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs#L1910-L4076) |
| Selection autoscroll | Above/below pane, hot-zone drag versus click, safe-zone cancellation | [selection tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/selection.rs#L171-L300) |
| Clipboard selection | Ctrl+C interception, copied bytes, selection clearing, repeat suppression, normal Ctrl+C passthrough | [clipboard tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/clipboard.rs#L59-L185) |
| Thin-client lifecycle | Initial frames, server loss, terminal restoration after EOF/SIGHUP, graceful shutdown message, notification forwarding | [client-mode tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/tests/client_mode.rs#L295-L990) |
| Multi-client integration | Simultaneous attach, foreground sizing, non-foreground projection purity, broadcast, disconnect resize, crash/stress | [multi-client tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/tests/multi_client.rs#L772-L1141) |
| Detach/reattach | Prefix detach, explicit detach, server persistence, state on reattach, process survival, detached output and size | [detach tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/tests/detach_reattach.rs#L256-L738) |
| Cross-area flow | Client/API consistency, two-client shared view, detach stability, restart/reconnect | [cross-area tests](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/tests/cross_area.rs#L684-L1070) |

`Observation:` Most interaction tests construct pure `AppState` plus test terminal runtimes and directly feed semantic events. End-to-end PTY/socket tests are reserved for boundaries that cannot be established from pure state: terminal cleanup, protocol frames, resize, detach persistence, and multi-client ownership.

## Reusable Herdr patterns, stated as raw findings

These are patterns present in Herdr, not recommendations for another codebase.

1. **Compute geometry, then draw:** one pre-render phase owns normalization, runtime resize, and hit areas; drawing reads immutable state.
2. **Explicit interaction modes:** the top-level enum makes modal capture and mode transitions inspectable and unit-testable.
3. **Layered input arbitration:** popup, overlay, application mouse protocol, TUI chrome, selection, and PTY paths have an explicit precedence order.
4. **Stable input ownership:** client/input-source IDs and terminal IDs survive focus changes; leases preserve press/repeat/release consistency.
5. **Protocol-aware forwarding:** child modes decide keyboard release encoding, bracketed paste, focus events, mouse reporting, alternate scroll, and PageUp interception.
6. **Semantic actions after hit-testing:** mouse code returns actions such as focus/move/settings rather than performing every runtime mutation inline.
7. **Dynamic help from effective bindings:** the help UI shows custom and resolved bindings, including unset actions and compressed 1–9 ranges.
8. **Targeted notifications:** a toast contains stable domain identity and doubles as navigation to the affected pane.
9. **Latest-visual-state backpressure:** ordered control messages are not conflated with disposable stale frames.
10. **Pure-state tests plus boundary integration tests:** gesture logic is exercised without a real terminal; cleanup, socket, PTY, and multi-client contracts use integration tests.

## Cautionary Herdr patterns and observed tradeoffs

These are source observations and uncertainties, not a prescriptive plan.

1. **Resize still polls.** The platform signal is combined with a dedicated 100 ms polling loop. This adds bounded detection delay and an always-waking thread. [Source](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L2240-L2277).
2. **The thin client loop also has a 100 ms timer.** It helps notice the atomic quit flag but wakes in otherwise idle periods. [Source](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L1393-L1398).
3. **Top-level interaction logic remains broad.** `Mode` is explicit, but `mouse.rs`, `navigate.rs`, and `modal.rs` still centralize many unrelated surfaces and long precedence chains. This makes order changes consequential even when each branch is tested.
4. **Whole-mode IME switching is coarse.** The source acknowledges that Navigator and KeybindHelp search are incorrectly held to ASCII because `Mode::wants_ascii_input` cannot see search focus. [Source](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/state.rs#L846-L855).
5. **Attention priority is repeated.** The blocked/done/working/idle/unknown ordering appears in workspace aggregation, sidebar ordering, and API helper projection rather than one visibly shared function. [Workspace](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/workspace/aggregate.rs#L75-L100), [sidebar](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui/sidebar.rs#L241-L248), [API helper](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/api_helpers.rs#L99-L109).
6. **Foreground-client semantics are inherently complex.** A client interaction can change shared size, keymap, host theme, outer focus, and render behavior before its event is routed. Herdr has extensive tests around this, indicating it is a high-risk boundary. [Routing](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs#L2678-L2735).
7. **Selection timing is scheduled polling by deadline.** Drag autoscroll intentionally recurs every 30 ms while active. It is localized and stops when the gesture ends, but it is still time-driven state. [Source](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/state.rs#L70-L89).
8. **System clipboard output is OSC 52.** Success depends on the outer terminal accepting OSC 52; source inspection does not establish behavior across terminal security policies. [Source](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs#L2052-L2066).
9. **Suspend/job-control behavior is not established.** No explicit SIGTSTP/SIGCONT handling was found, and no live probe was performed.
10. **Documentation path maturity varies.** The keyboard guide used here is under `docs/next`; behavior claims were checked against production source rather than accepted from the guide alone.

## Source map

| File | Interaction responsibility |
| --- | --- |
| [`src/client/mod.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/mod.rs) | Host terminal setup/restore, client threads, frame blit, resize, detach, clipboard/image bridge, direct attach |
| [`src/client/input.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/client/input.rs) | Unix raw framing and Windows semantic acquisition |
| [`src/server/client_transport.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/client_transport.rs) | Socket messages, payload limits, ordered control and one-slot render queues |
| [`src/server/headless.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/headless.rs) | Foreground-client lease, routing, resize, notifications, rendering, detach/shutdown |
| [`src/server/render_stream.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/server/render_stream.rs) | Virtual Ratatui backend and per-client semantic/ANSI baselines |
| [`src/app/state.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/state.rs) | Modes, view geometry, drag/selection/modal/notification state, interaction configuration |
| [`src/app/runtime.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/runtime.rs) | Async raw-input batches, key leases, focus events, scheduled deadlines |
| [`src/app/input/mod.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mod.rs) | Top-level key/text/paste/mouse dispatch |
| [`src/app/input/terminal.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/terminal.rs) | Direct binding interception, child protocol encoding, stable targets, key release |
| [`src/app/input/navigate.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/navigate.rs) | Prefix/navigation state machines, actions, custom commands, scrollback editor |
| [`src/app/input/mouse.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/mouse.rs) | Chrome/pane hit-testing, gestures, reorder, resize, menus, wheel routing |
| [`src/app/input/selection.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/selection.rs) | Drag selection and edge autoscroll |
| [`src/app/input/copy_mode.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/copy_mode.rs) | Pane-pinned keyboard copy mode, search, selection, focus/resize synchronization |
| [`src/app/input/modal.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/modal.rs) | Shared modal actions, global menu, navigator key handling |
| [`src/app/input/overlays.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/app/input/overlays.rs) | Overlay-specific mouse capture and scrollbars |
| [`src/config/keybinds.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/config/keybinds.rs) | Binding grammar, conflicts, direct/prefix/indexed/custom resolution |
| [`src/input/model.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/input/model.rs) | Semantic key identity and source metadata |
| [`src/input/encode.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/input/encode.rs) | Kitty/legacy keyboard and mouse encoding |
| [`src/pane.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/pane.rs#L2660-L2730) | Pane runtime input channel and bracketed-paste payload |
| [`src/pane/terminal.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/pane/terminal.rs#L113-L139) | Child terminal input-mode snapshot and PageUp arbitration predicate |
| [`src/ui.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui.rs) | Compute/render split, responsive layout, overlay render order |
| [`src/ui/keybind_help.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui/keybind_help.rs) | Effective searchable keymap presentation |
| [`src/ui/status.rs`](https://github.com/herdrdev/herdr/blob/2863b715132fe29e53089e06f105943d1df0b3b4/src/ui/status.rs) | Status symbols, text labels, notification rendering |
