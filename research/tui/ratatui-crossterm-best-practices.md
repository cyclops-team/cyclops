# Production Rust TUI practices with Ratatui and Crossterm

Research date: 2026-08-05

This report records primary-source research into production TUI design with
Ratatui and Crossterm. It deliberately does not compare implementations or
recommend a migration for a particular application.

The report uses two labels:

- **Source fact** describes behavior documented by the named project or
  standard.
- **Recommendation (inference)** is a design conclusion drawn from those
  sources. It is not a claim that Ratatui or Crossterm requires that design.

## Version baseline and applicability

The repository baseline at the research date is:

| Dependency | Resolved version | Relevant detail |
|---|---:|---|
| Ratatui | 0.30.2 | Default features include the Crossterm backend and layout cache. |
| `ratatui-crossterm` | 0.1.2 | Ratatui's backend adapter. |
| Direct Crossterm dependency | 0.28.1 | Used by the workspace crate. |
| Crossterm through Ratatui | 0.29.0 | A second resolved Crossterm version. |
| Tokio | 1.53.1 | Async runtime and channels. |
| `unicode-segmentation` | 1.13.3 | Extended grapheme-cluster segmentation. |
| `unicode-width` | 0.2.2 | Terminal-cell width calculation. |

The local evidence at the recorded repository revision is the
[`cyclops-workspace` manifest](https://github.com/cyclops-team/cyclops/blob/3b5c768eb6f2d03337d50fb0bae305f8f19eab35/crates/cyclops-workspace/Cargo.toml)
and [`Cargo.lock`](https://github.com/cyclops-team/cyclops/blob/3b5c768eb6f2d03337d50fb0bae305f8f19eab35/Cargo.lock).
Ratatui's feature mapping is documented in
its [0.30.2 crate manifest](https://docs.rs/crate/ratatui/0.30.2/source/Cargo.toml.orig),
and its backend package recommends using Ratatui's `ratatui::crossterm`
re-export to avoid feature and version mismatches
([`ratatui-crossterm` 0.1.2](https://docs.rs/ratatui-crossterm/0.1.2/ratatui_crossterm/)).

**Recommendation (inference).** A TUI should resolve one Crossterm version at
its terminal boundary. Either align the direct dependency with Ratatui's
selected backend version, select Ratatui's `crossterm_0_28` feature, or import
terminal types consistently through `ratatui::crossterm`. Two versions are
distinct Rust types and make lifecycle and event code easier to misuse. This
is the first compatibility issue to check before applying examples from newer
documentation.

The remainder of this report cites the exact 0.30.2/0.28.1 APIs where the
version matters. Advice based on newer optional APIs is marked explicitly.

## Reference architecture

Ratatui intentionally renders widgets but does not own input handling. Its
official material describes both a centralized event loop and application
patterns based on the Elm architecture and components
([event handling](https://ratatui.rs/concepts/event-handling/),
[Elm architecture](https://ratatui.rs/concepts/application-patterns/the-elm-architecture/),
[component architecture](https://ratatui.rs/concepts/application-patterns/component-architecture/)).

**Recommendation (inference).** Use one authoritative model and a one-way
flow:

```text
terminal / worker / timer events
              |
              v
        normalize to Event
              |
              v
       route to an Action
              |
              v
  update(model, action) -> effects
              |
              v
       render the model
```

The properties that matter are:

1. One task owns mutable UI state.
2. Input, daemon notifications, worker completions, resize, and shutdown are
   normalized before state mutation.
3. `update` is deterministic and can be tested without a terminal.
4. Rendering reads state but does not initiate work or mutate domain state.
5. Async effects return typed actions rather than mutating widgets directly.
6. Exactly one owner draws and changes terminal modes.

This follows Ratatui's model/update/view description and its component
template's separation of event handling, action dispatch, and rendering
([component template](https://ratatui.rs/templates/component/),
[`components.rs` walkthrough](https://ratatui.rs/templates/component/components-rs/)).
It is a recommendation, not a requirement to copy the template's exact trait
or channel hierarchy.

### Events, actions, and effects

**Recommendation (inference).** Keep these concepts separate:

- An **event** is an observation: `Key`, `Paste`, `Resize`, `WorkerFinished`,
  `ConnectionLost`, or `ShutdownRequested`.
- An **action** is user intent: `MoveSelection`, `OpenHelp`, `Confirm`,
  `Cancel`, or `SubmitInput`.
- An **effect** is external work requested by an update: send a command,
  load data, or start a cancellable operation.

This prevents terminal-specific details such as key codes from becoming
business state and lets multiple inputs invoke the same intent. It also makes
help text, remapping, mouse input, and tests share one action vocabulary. The
official Ratatui event and component guides support this separation, while
leaving the concrete types to the application
([event handling](https://ratatui.rs/concepts/event-handling/)).

Components may own local presentation state, but shared selection, focus,
modal state, and data identity should have one owner. Otherwise two components
can accept the same key or render mutually inconsistent state, a failure mode
the centralized event guide calls out.

## Terminal lifecycle and restoration

### What the libraries provide

**Source fact.** Ratatui 0.30.2 provides `ratatui::run`, which initializes the
terminal, executes a closure, and restores the terminal on normal return and
panic. `ratatui::init` and `restore` expose the same standard lifecycle when
the caller needs more control
([`run`](https://docs.rs/ratatui/0.30.2/ratatui/fn.run.html),
[`init` module](https://docs.rs/ratatui/0.30.2/ratatui/init/index.html),
[`init`](https://docs.rs/ratatui/0.30.2/ratatui/fn.init.html)). A manually
constructed `Terminal` does not itself enable raw mode, enter the alternate
screen, or install a panic hook
([`Terminal`](https://docs.rs/ratatui/0.30.2/ratatui/struct.Terminal.html)).

**Source fact.** Crossterm raw mode disables normal line editing and echo and
changes special-key handling, including the terminal driver's usual Ctrl-C
processing. The alternate screen preserves the main screen but has no normal
scrollback
([Crossterm terminal module](https://docs.rs/crossterm/0.28.1/crossterm/terminal/index.html)).

**Recommendation (inference).** Prefer `ratatui::run` for a standard
full-screen application. Use a dedicated RAII lifecycle guard when the
application additionally enables mouse capture, focus events, bracketed
paste, keyboard enhancements, cursor changes, or inline/suspend behavior.
Ratatui's standard restoration covers its standard modes; it cannot restore
optional modes it did not enable.

### Treat setup as a transaction

Terminal setup can fail after only some modes have been enabled. A robust
guard should therefore record each successful acquisition and undo only those
steps. It should provide:

- an explicit `finish`/`restore` path that can return a cleanup error;
- an idempotent, best-effort `Drop` fallback;
- panic restoration;
- restoration before printing a fatal error to the main screen;
- suspend or shell-out hooks that restore before yielding the terminal and
  reacquire modes afterward.

This is a **recommendation (inference)** from the fallible setup/restore APIs.
Ratatui exposes `try_init` and `try_restore` when the caller needs errors,
while its non-`try` restoration reports cleanup problems rather than returning
them
([initialization API](https://docs.rs/ratatui/0.30.2/ratatui/init/index.html)).

**Recommendation (inference).** Cleanup must preserve the original failure.
If drawing fails and restoration also fails, report the draw error as primary
and attach the cleanup error as context. A `Drop` implementation cannot return
an error, so explicit restoration remains necessary on ordinary `Result`
paths.

### Panic hooks are global

**Source fact.** `std::panic::set_hook` changes a global hook, and the hook is
invoked before either unwinding or aborting
([Rust `set_hook`](https://doc.rust-lang.org/std/panic/fn.set_hook.html)).
Ratatui's lifecycle documentation explains that an application hook should be
installed before Ratatui initialization so Ratatui can restore the terminal
before delegating to it.

**Recommendation (inference).** Install and chain a panic hook once, at the
process boundary. Keep its restoration path small and idempotent. Do not let
individual components replace it, and do not make the hook the only cleanup
path.

### Signals and controlled shutdown

**Recommendation (inference).** Because raw mode changes Ctrl-C behavior,
handle `Ctrl-C` as a key action inside the event router. Handle external
termination signals separately where the platform supports them, route both
to the same controlled shutdown action, stop background work, restore the
terminal, and only then print final diagnostics. Tokio supplies async signal
handling, but installing its Ctrl-C handler also changes the process's default
signal behavior and must be treated as process-wide configuration
([Tokio `ctrl_c`](https://docs.rs/tokio/1.53.1/tokio/signal/fn.ctrl_c.html)).

## Crossterm event modes and input acquisition

### Choose one event acquisition strategy

**Source fact.** Crossterm 0.28.1 permits either:

- `read`/`poll` from the same thread; or
- `EventStream`.

It explicitly forbids calling `read` and `poll` on different threads or
combining them with `EventStream`. `read` blocks until an event is available
([Crossterm event module](https://docs.rs/crossterm/0.28.1/crossterm/event/index.html)).

**Recommendation (inference).** Give terminal input exactly one owner. A
blocking reader on one long-lived thread is suitable for a synchronous loop.
For an async loop that must select input with cancellation, workers, signals,
and deadlines, enable Crossterm's optional `event-stream` feature and consume
one `EventStream`. Do not add a second input reader to work around shutdown.

`EventStream` is a `Stream<Result<Event>>`; in 0.28.1 it is feature-gated and
implemented with a helper thread
([API](https://docs.rs/crossterm/0.28.1/crossterm/event/struct.EventStream.html),
[0.28.1 source](https://docs.rs/crate/crossterm/0.28.1/source/src/event/stream.rs)).
The feature is not implied merely by using Crossterm for ordinary event reads.

### Optional terminal modes must be paired

**Source fact.** Mouse capture and focus-change events are disabled by
default. Bracketed paste also has explicit enable and disable commands.
Crossterm documents paired commands for all three modes
([event module](https://docs.rs/crossterm/0.28.1/crossterm/event/index.html)).
Keyboard enhancement flags similarly use push/pop commands
([keyboard flags](https://docs.rs/crossterm/0.28.1/crossterm/event/struct.KeyboardEnhancementFlags.html)).

**Recommendation (inference).** Enable only modes with a concrete feature
that consumes them, record every successful enable in the lifecycle guard,
and disable in cleanup. Optional terminal protocols vary across emulators, so
unsupported enhancements must degrade to ordinary key handling rather than
make the application unusable.

### Key press, repeat, and release

**Source fact.** A `KeyEvent` contains code, modifiers, kind, and state.
`KeyEventKind` distinguishes press, repeat, and release. On Unix, enhanced
event kinds generally require terminal keyboard protocol support; Windows can
report the kinds directly
([`KeyEvent`](https://docs.rs/crossterm/0.28.1/crossterm/event/struct.KeyEvent.html),
[`KeyEventKind`](https://docs.rs/crossterm/0.28.1/crossterm/event/enum.KeyEventKind.html)).

**Recommendation (inference).** Bind commands on `Press`, ignore `Release`
for activation, and explicitly decide which navigation actions may accept
`Repeat`. Never assume every platform reports identical event sequences.
Tests should include press/repeat/release so enhanced keyboard mode does not
double-activate commands.

### Focus and mouse events

**Recommendation (inference).** Keep terminal focus (`FocusGained` and
`FocusLost`) distinct from in-application component focus. A focus loss can
pause optional animation or suppress hover, but should not silently change
selection or submit data.

Crossterm mouse coordinates are terminal-relative fields on `MouseEvent`
([`MouseEvent`](https://docs.rs/crossterm/0.28.1/crossterm/event/struct.MouseEvent.html)).
Route them against rectangles produced by the same layout pass as the visible
frame. Never duplicate layout arithmetic in a separate mouse subsystem. Mouse
move and drag are replaceable high-volume events; coalesce them to the latest
position and retain keyboard equivalents for every action.

## Event-driven async work and cancellation

### Do not turn blocking input into an uncancellable task

**Source fact.** Once a Tokio `spawn_blocking` task starts, aborting its join
handle does not stop it. Runtime shutdown can wait indefinitely for blocking
tasks
([Tokio `spawn_blocking`](https://docs.rs/tokio/1.53.1/tokio/task/fn.spawn_blocking.html)).

**Recommendation (inference).** Do not use
`spawn_blocking(crossterm::event::read)` as though it were cancellable. Use a
process-lifetime reader thread with an explicit ownership contract, or a
single `EventStream` in an async task. The latter can participate in
`tokio::select!` alongside shutdown and worker channels.

### Cancellation has one owner

**Source fact.** `tokio::select!` waits on several async branches and cancels
the unselected branch futures by dropping them. Its documentation lists which
Tokio receive/read operations are cancellation-safe and warns that some
operations can lose progress
([Tokio `select!`](https://docs.rs/tokio/1.53.1/tokio/macro.select.html)).

**Recommendation (inference).** Establish one top-level shutdown signal. On
shutdown:

1. stop accepting new effects;
2. signal long-lived tasks;
3. await or join tasks whose completion matters;
4. abandon only tasks documented as safe to abandon;
5. restore the terminal;
6. report the final result.

Audit every future placed in `select!` for cancellation safety. A dropped
future must not lose a partially decoded frame, half-written command, or
exclusive resource. If a reusable cancellation primitive is desired,
`tokio_util::sync::CancellationToken` provides child tokens and a cancellation
future, but `tokio-util` is an additional dependency rather than a Tokio core
API
([`CancellationToken`](https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html)).

### Backpressure and replaceable events

Tokio's bounded MPSC channel supplies backpressure; its unbounded channel can
buffer without a fixed limit
([Tokio MPSC](https://docs.rs/tokio/1.53.1/tokio/sync/mpsc/index.html)).

**Recommendation (inference).** Classify messages before choosing a channel:

| Class | Examples | Policy |
|---|---|---|
| Lossless control | quit, command result, connection transition | bounded queue; apply backpressure or reserve capacity |
| Latest value wins | resize, mouse move, progress, snapshot | coalesce or use a watch/latest-value channel |
| Bursty data | logs, stream records | bounded queue with an explicit drop/spill policy |
| Periodic animation | spinner/frame deadline | no queue; derive from a one-shot deadline |

Unbounded queues should be reserved for traffic proven to be both bounded and
low volume. Record dropped or coalesced counts so overload is visible.

### Await events; do not run a permanent tick loop

`crossterm::event::read` blocks until input and `EventStream` wakes when input
arrives. Tokio sleeps are also dormant until their deadline and are cancelled
by being dropped
([Crossterm event module](https://docs.rs/crossterm/0.28.1/crossterm/event/index.html),
[Tokio `sleep`](https://docs.rs/tokio/1.53.1/tokio/time/fn.sleep.html)).

**Recommendation (inference).** Await real sources: terminal input, data
changes, worker completion, shutdown, and a one-shot animation deadline. Mark
the model dirty when an event changes visible state and draw once after a
burst. Do not wake at a fixed frame rate when nothing is animated.

For animation, schedule the next required deadline with `sleep_until`. For
bursts, coalesce until a short, fixed maximum deadline and then render the
latest model; do not keep extending the deadline indefinitely, because a
continuous stream would starve the screen.

## Input routing, keybindings, focus, and modal layers

### Deterministic routing

**Recommendation (inference).** Resolve an input exactly once, in this order:

1. process-lifecycle or emergency commands;
2. the topmost modal/menu;
3. the focused component;
4. non-conflicting global bindings;
5. optional pass-through to an embedded terminal or child application.

Once a layer consumes an event, stop. Text-entry focus must prevent printable
global shortcuts from stealing characters. Modal input must never leak to the
obscured view. This applies the official centralized event guidance and the
WAI-ARIA dialog's keyboard/focus behavior as a transferable UI heuristic
([Ratatui event handling](https://ratatui.rs/concepts/event-handling/),
[WAI-ARIA modal dialog pattern](https://www.w3.org/WAI/ARIA/apg/patterns/dialog-modal/)).

### Bind actions, not handlers

**Recommendation (inference).** Store bindings as data from normalized key
chords to actions. Use that same table to render contextual help, validate
configuration, and drive tests. Detect duplicates and shadowing when loading
configuration. Binding display should be generated from the normalized chord,
not maintained as separate prose.

Global single-character shortcuts are hazardous in text fields and can also
be triggered accidentally by speech input. Scope them to non-editing focus or
make them remappable. W3C's single-character shortcut guidance recommends a
way to turn off, remap, or limit such shortcuts to focused controls
([WCAG 2.2 shortcut guidance](https://www.w3.org/WAI/WCAG22/Understanding/character-key-shortcuts)).
The WCAG source concerns web content; the risk and mitigation are useful TUI
design heuristics, not a claim of TUI conformance.

### One visible focus owner

**Recommendation (inference).** Model focus explicitly. Selection and focus
are different: a list may retain a selected row while a search box has input
focus. At any moment, exactly one interactive target should own keyboard
focus, and the view should show it without relying on color alone.

Opening a modal should remember the previous focus, choose a safe initial
target, trap focus within the modal, allow Escape to close one layer, and
restore focus to the invoker. For irreversible actions, default focus should
favor the least destructive option. These are transferable practices from
the WAI-ARIA dialog and keyboard-interface guidance
([modal dialog](https://www.w3.org/WAI/ARIA/apg/patterns/dialog-modal/),
[keyboard interface](https://www.w3.org/WAI/ARIA/apg/practices/keyboard-interface/),
[focus visibility](https://www.w3.org/WAI/WCAG22/Understanding/focus-visible)).

## Layout, resize, and responsive behavior

### Build from the frame's actual area

**Source fact.** Ratatui's `Layout` divides a `Rect` using constraints and is
cached when the default `layout-cache` feature is enabled. Percentage and
ratio constraints are based on the entire input area, not on the remainder
after fixed constraints; `Fill` absorbs available space
([`Layout`](https://docs.rs/ratatui/0.30.2/ratatui/layout/struct.Layout.html),
[`Constraint`](https://docs.rs/ratatui/0.30.2/ratatui/layout/enum.Constraint.html),
[`Flex`](https://docs.rs/ratatui/0.30.2/ratatui/layout/enum.Flex.html)).

**Recommendation (inference).** Begin every layout at `frame.area()`, not at
an independently queried terminal size and not at origin `(0, 0)`. The area
can have a nonzero origin for inline or fixed viewports. Use Ratatui layout
constraints for the structural split and saturating/clamped arithmetic for
small decorative calculations.

### Define compact modes, not just ideal dimensions

Every terminal size is valid input, including zero-sized areas during startup
or a resize race. Define explicit breakpoints:

- normal: all primary regions visible;
- compact: secondary labels/help collapse;
- minimal: one primary region plus a short recovery message;
- too small: render a bounded size requirement without panicking.

This is a **recommendation (inference)** based on Ratatui's rectangle model.
Prefer reflowing or hiding lower-priority information to producing many
zero-width rectangles. Layout tests should include `0x0`, `1x1`, every
breakpoint boundary, narrow/tall, wide/short, and very large sizes.

### Resize events are state changes

**Source fact.** Crossterm exposes `Event::Resize(width, height)` and notes
that multiple resize events can arrive as a batch
([`Event`](https://docs.rs/crossterm/0.28.1/crossterm/event/enum.Event.html)).
Ratatui's `Terminal::draw` autoresizes fullscreen and inline viewports; fixed
viewports require explicit sizing
([`Terminal`](https://docs.rs/ratatui/0.30.2/ratatui/struct.Terminal.html)).

**Recommendation (inference).** Retain only the latest pending size, invalidate
layout and hit-test rectangles, and render once after the burst. Never discard
the latest resize. Do not assume a resize event and the next backend size
query are perfectly synchronized.

### Hit testing uses rendered geometry

**Recommendation (inference).** During layout/render, retain the final
interactive rectangles in a hit map identified by stable component IDs.
Replace the hit map each frame. Mouse handlers then use exactly what the user
saw, including compact-mode omissions, scroll offsets, borders, overlays, and
nonzero viewport origins.

## Rendering correctness and performance

### Render a complete frame

**Source fact.** Ratatui uses a double-buffered immediate-mode model. Each draw
renders the current frame, diffs it against the previous buffer, writes only
changed cells, swaps buffers, and flushes the backend
([`Terminal::draw`](https://docs.rs/ratatui/0.30.2/ratatui/struct.Terminal.html)).

**Recommendation (inference).** Render the complete visible state on every
draw. Do not maintain a separate dirty-rectangle renderer or directly print
incremental UI updates outside Ratatui. Direct backend writes desynchronize
Ratatui's remembered buffer; if they are unavoidable, clear or force a full
redraw before trusting diffs again.

Only one task should call `draw`. Logging, panic output, and workers must not
write to the same terminal while the full-screen UI is active.

### Draw on visible change

Ratatui reduces terminal output per draw, but layout, widget rendering, buffer
comparison, allocation, and flushing still cost work.

**Recommendation (inference).** Separate “an event arrived” from “the visible
model changed.” Draw on initial display, visible state mutation, latest
resize, or an active animation deadline. Batch several ready actions before
one draw, while bounding latency. Cache expensive derived presentation data by
model revision, not by attempting to cache terminal cells.

For large collections, format and render only the visible window plus a small
overscan. Preserve selection by stable item identity rather than screen row.
Benchmark worst-case width, height, and data volume; averages hide terminal
hot paths.

### Popups and z-order

Ratatui renders later widgets over earlier cells and supplies a `Clear` widget
for clearing an overlay area
([`Clear`](https://docs.rs/ratatui/0.30.2/ratatui/widgets/struct.Clear.html)).

**Recommendation (inference).** Render in explicit layers: base, chrome,
overlays, then the top modal. Clear or fully paint a popup's area so wide
characters and prior styles do not bleed through. Keep the visual layer order
identical to the input-routing order.

### Cursor behavior

**Recommendation (inference).** Hide the hardware cursor for navigation-only
views. For text editing, place it at the final display-cell position after
rendering, keep it inside `frame.area()`, and make its presence agree with the
focus model. A cursor on an unfocused or obscured field is a misleading second
focus indicator. Ratatui exposes cursor positioning on `Frame`
([`Frame`](https://docs.rs/ratatui/0.30.2/ratatui/struct.Frame.html)).

### Render failures are session-fatal by default

Ratatui documents that a draw error may leave the terminal with a partially
drawn frame
([`Terminal`](https://docs.rs/ratatui/0.30.2/ratatui/struct.Terminal.html)).

**Recommendation (inference).** Treat backend draw/flush failure as fatal to
the current TUI session unless a recovery protocol explicitly clears and
reinitializes both terminal state and buffers. Attempt cleanup and report the
error on the restored main screen.

## Unicode, width, truncation, and editing

### Bytes, scalar values, graphemes, and cells differ

Unicode defines extended grapheme clusters as an approximation of
user-perceived characters and recommends them for cursor movement, selection,
and deletion boundaries
([Unicode UAX #29](https://unicode.org/reports/tr29/)). The resolved Ratatui
stack uses `unicode-segmentation` and `unicode-width`; Ratatui `Span` width is
measured in display columns and its styled-grapheme iteration uses extended
graphemes
([Ratatui `Span`](https://docs.rs/ratatui/0.30.2/ratatui/text/struct.Span.html),
[`unicode-segmentation`](https://docs.rs/unicode-segmentation/1.13.3/unicode_segmentation/trait.UnicodeSegmentation.html)).

**Recommendation (inference).** Never truncate, index, delete, or position a
cursor by UTF-8 byte or Rust `char` count. Keep editor offsets on extended
grapheme boundaries and calculate visual offsets in terminal cells. The data
model may retain byte offsets for efficient slicing, but every stored offset
must be validated as a grapheme boundary.

Test at least:

- combining marks, such as a decomposed accented letter;
- emoji joined with zero-width joiners;
- variation selectors and skin tones;
- regional-indicator flags;
- CJK wide characters;
- an empty string and leading combining marks;
- truncation immediately before and after a two-cell grapheme.

### Width is an estimate, not a universal truth

`unicode-width` reports a normal width and a CJK width; ambiguous characters
are one cell in the former and two in the latter
([`UnicodeWidthStr`](https://docs.rs/unicode-width/0.2.2/unicode_width/trait.UnicodeWidthStr.html)).
Unicode's East Asian Width annex explains that actual display depends on
context, font, and terminal behavior
([Unicode UAX #11](https://unicode.org/reports/tr11/)).

**Recommendation (inference).** Use one width policy consistently across
layout, truncation, cursor placement, and hit testing, and test it in the
supported terminal emulators. Expect occasional terminal/library disagreement
for ambiguous or emoji width. Do not make an emoji the only alignment anchor
or the only carrier of meaning; provide an ASCII-safe fallback for chrome and
status glyphs.

Ratatui's normal width path is not automatically a locale-sensitive CJK width
policy. If an application chooses a different width profile, it must apply it
coherently rather than fixing individual strings.

### Truncation preserves cells and meaning

**Recommendation (inference).** Truncate only between grapheme clusters and
reserve the ellipsis width before filling the line. Never split a two-cell
symbol at the last column. For operational values such as identifiers or error
causes, prefer scrolling, wrapping, or a details view to silently removing the
distinguishing suffix. A tooltip that requires a mouse is not an adequate sole
recovery path.

## Color, capability, and semantic styling

### Capabilities are limited and heuristic

**Source fact.** Crossterm 0.28.1 exposes `available_color_count`, which uses
terminal environment information to report an available color level
([`available_color_count`](https://docs.rs/crossterm/0.28.1/crossterm/style/fn.available_color_count.html)).
Such environment-based detection is a capability hint, not negotiation with
every emulator.

**Recommendation (inference).** Define semantic tokens such as `normal`,
`muted`, `focus`, `warning`, `error`, `success`, and `selection`, then map them
once to truecolor, 256-color, 16-color, and monochrome palettes. Renderers
should request tokens, never embed raw RGB decisions. Allow an explicit user
override because automatic detection can be wrong through SSH, tmux, CI, and
unusual emulators.

Prefer the terminal's default foreground/background for broad surfaces unless
the user selected a complete theme. A palette must remain legible on both dark
and light terminal backgrounds.

### Respect `NO_COLOR`

The `NO_COLOR` convention says a present, nonempty environment variable
disables color by default; user configuration or an explicit command-line
request may override it. The convention concerns color, not all text
attributes
([NO_COLOR](https://no-color.org/)).

**Recommendation (inference).** Apply `NO_COLOR` at semantic palette
selection, not in individual widgets. Test that the monochrome rendering has
the same information and contains no color escapes. Do not remove useful
weight or underline solely because color is off, but ensure those attributes
also degrade safely.

### Color never carries state alone

W3C's use-of-color guidance requires another visual means when color conveys
information
([WCAG 2.2 use of color](https://www.w3.org/WAI/WCAG22/Understanding/use-of-color)).

**Recommendation (inference).** Duplicate every semantic color with a word,
glyph, border, position, or text attribute. Examples: `error` plus an error
label, focused plus an explicit focus marker, selected plus a reverse/bold
row, and connection state plus a stable status word. This is a transferable
accessibility principle; terminal palettes and fonts do not provide enough
control to claim web contrast conformance from a numeric theme value alone.

## Accessibility beyond color

Terminal UIs do not expose a web-style semantic accessibility tree merely by
using Ratatui. WAI-ARIA itself exists to add roles, states, and properties that
assistive technology can consume in platforms that support those semantics
([WAI-ARIA overview](https://www.w3.org/WAI/standards-guidelines/aria/)).

**Recommendation (inference).** Treat WCAG and APG sources as interaction
heuristics, not proof that a TUI is accessible. Validate actual behavior with
the supported terminal, OS, and screen reader combinations.

A practical baseline is:

- every function is keyboard-operable, with mouse as an optional parallel
  path ([WCAG keyboard technique](https://www.w3.org/WAI/WCAG22/Techniques/general/G202.html));
- focus is visible, singular, predictable, and restored after modal use;
- status and errors use stable words, not color, animation, or an icon alone;
- help is reachable from the keyboard and derived from current bindings;
- motion is limited to useful state and has a reduced/disabled mode;
- no essential content depends on hover;
- ASCII-safe glyphs and a monochrome theme are available;
- a stable, line-oriented or non-fullscreen output mode is available for
  screen readers, logs, pipes, and copied diagnostics.

Ratatui supports inline viewports as well as full-screen rendering
([`Viewport`](https://docs.rs/ratatui/0.30.2/ratatui/struct.Viewport.html)).
An inline or plain output mode can preserve scrollback and reduce continuous
screen rewrites, but it still needs manual assistive-technology testing.

## Bracketed paste and untrusted terminal text

**Source fact.** When bracketed paste is enabled, Crossterm emits a complete
`Event::Paste(String)` rather than making the application infer paste solely
from ordinary key events
([`Event`](https://docs.rs/crossterm/0.28.1/crossterm/event/enum.Event.html),
[event mode commands](https://docs.rs/crossterm/0.28.1/crossterm/event/index.html)).
That identifies the input as a paste; it does not make its contents trusted.

**Recommendation (inference).** Route `Paste` directly to the focused text
destination. Never replay its characters through the keybinding router. Apply
destination-specific limits and newline policy, and require confirmation if a
paste could cause execution or a destructive command.

Pasted and externally supplied text can include terminal control characters.
MITRE CWE-150 documents how escape sequences can manipulate a terminal and
spoof output
([CWE-150](https://cwe.mitre.org/data/definitions/150.html)). Ratatui's
Crossterm backend ultimately prints cell symbols
([backend source](https://docs.rs/crate/ratatui-crossterm/0.1.2/source/src/lib.rs)).

**Recommendation (inference).** At the trust boundary:

- reject, strip, or visibly encode C0/C1 controls, ESC, CSI, OSC, and other
  non-content sequences before rendering or logging;
- interpret allowed newlines/tabs in the application, not as raw terminal
  output;
- bound input by bytes and graphemes to prevent memory and layout abuse;
- never interpolate paste or UI text into a shell command string;
- pass external command arguments as distinct arguments;
- avoid recording raw paste, credentials, tokens, or private input in traces;
- pair bracketed-paste enable with disable in terminal cleanup.

Sanitize all untrusted displayed strings, not only paste: process output,
daemon data, file names, remote labels, and error messages can carry the same
escape sequences.

## Testing strategy

### Separate reducer, widget, and terminal tests

**Source fact.** Ratatui's `TestBackend` documentation recommends direct
`Buffer` tests for individual widgets and `TestBackend` for terminal-level
integration tests
([`TestBackend`](https://docs.rs/ratatui/0.30.2/ratatui/backend/struct.TestBackend.html),
[`Widget`](https://docs.rs/ratatui/0.30.2/ratatui/widgets/trait.Widget.html)).
Ratatui also publishes an official snapshot-testing recipe
([snapshot recipe](https://ratatui.rs/recipes/testing/snapshots/)).

**Recommendation (inference).** Use five layers:

1. **Reducer tests:** table-test `model + action -> model/effects`; include
   invalid/stale IDs, channel closure, cancellation, and error actions.
2. **Input-router tests:** test key kind, modifiers, text focus, modal priority,
   remapping, duplicate bindings, mouse rectangles, and paste bypass.
3. **Widget buffer tests:** render one component into a `Buffer` and assert
   symbols, styles, cursor intent, and clipping.
4. **`TestBackend` integration/snapshots:** render a complete view and exercise
   actions across a matrix of terminal sizes, focus states, modal stacks,
   themes, and Unicode samples.
5. **PTY/real-terminal smoke tests:** verify raw/alternate/mouse/paste cleanup,
   panic restoration, resize bursts, signals, capability fallbacks, and the
   supported emulators.

Snapshots should supplement semantic assertions. Review snapshot changes as
behavior changes; never update them blindly. A snapshot can show changed
cells but cannot prove focus routing, cleanup, actual emulator width, or
assistive-technology behavior.

### Property and state-machine tests

Property testing is useful when terminal sizes, Unicode strings, action
sequences, and modal stacks create a large state space. Proptest's official
guide includes state-machine testing patterns
([Proptest state machines](https://proptest-rs.github.io/proptest/proptest/state-machine.html)).

**Recommendation (inference).** Generate sizes, input sequences, and text and
assert invariants:

- render/update never panics;
- every rectangle stays within its parent;
- cursor and hit-test positions stay inside the frame;
- exactly one valid focus owner exists;
- a top modal prevents underlying actions;
- close restores a valid previous focus;
- selection remains valid after data deletion/reorder;
- grapheme truncation returns valid UTF-8 and fits the cell budget;
- coalescing retains the latest resize/value;
- shutdown is idempotent.

Minimized failing sequences are especially valuable for focus and modal bugs
that are hard to reproduce by hand.

### Failure-injection tests

**Recommendation (inference).** Make terminal setup, event read, draw, worker
send, and cleanup failure injectable behind narrow interfaces. Verify partial
setup rollback, primary-error preservation, worker panic conversion, channel
closure, and terminal restoration. `TestBackend` covers rendering logic; a
PTY or fake terminal-mode adapter is needed for lifecycle assertions.

## Error handling and observability

### Make failures typed events

**Recommendation (inference).** Convert event-source errors, channel closure,
worker completion, worker panic, and recoverable domain errors into typed app
events. The reducer decides whether to show an inline error, retry, or begin
fatal shutdown. Avoid panics for user data, terminal size, optional protocol
support, and expected connection loss.

A fatal error path should:

1. stop new effects;
2. capture the primary error and relevant structured context;
3. cancel/join owned work;
4. restore terminal modes;
5. print one concise message and the log location on the main screen;
6. return a nonzero exit status.

Draw failures are fatal by default because the displayed frame may be partial.
Cleanup continues best-effort even after another cleanup step fails.

### Keep diagnostics out of the active screen

The `tracing` crate provides structured events and spans that preserve causal
context across async work
([`tracing`](https://docs.rs/tracing/0.1.44/tracing/)).

**Recommendation (inference).** Initialize diagnostics before entering the
alternate screen. During TUI operation, send detailed logs to a file or other
non-terminal sink; do not interleave stderr/stdout with Ratatui. Keep a
nonblocking appender's guard alive until after final flush and restoration;
the official `tracing-appender` docs note that dropping its guard loses the
flush guarantee and that a lossy writer may drop events under pressure
([`WorkerGuard`](https://docs.rs/tracing-appender/latest/tracing_appender/non_blocking/struct.WorkerGuard.html)).

Instrument, without recording sensitive content:

- event-source lifecycle and errors;
- queue depth/high-water marks;
- dropped and coalesced counts by event class;
- reducer/update latency;
- draw duration and draw cause;
- resize dimensions and compact-mode transitions;
- effect start/completion/cancellation;
- reconnect/backoff transitions;
- shutdown phase and cleanup failures.

Do not render on every log record. Logging is an observation stream, not an
implicit UI event, unless the visible model explicitly includes a bounded log
view.

## Prioritized implementation checklist

This checklist is generic. It is not a project-specific adoption plan.

### P0: correctness, restoration, and data safety

- [ ] Resolve one Crossterm version at the terminal boundary; prefer
  `ratatui::crossterm` or an explicitly aligned feature/version.
- [ ] Give one task ownership of the model, terminal drawing, and terminal
  modes.
- [ ] Use `ratatui::run` or a transactional RAII guard with explicit and `Drop`
  cleanup.
- [ ] Restore raw mode, alternate screen, cursor, mouse, focus, paste, and
  keyboard enhancements on normal error, panic, signal, and suspend/shell-out.
- [ ] Use exactly one Crossterm input strategy; never mix `read`/`poll` with
  `EventStream`.
- [ ] Do not treat `spawn_blocking(event::read)` as cancellable.
- [ ] Route terminal/worker/timer input to typed actions and update one model.
- [ ] Make modal routing exclusive and text-entry routing immune to printable
  global shortcuts.
- [ ] Treat draw failure as fatal to the session and preserve the primary
  error through cleanup.
- [ ] Treat paste and every external display string as untrusted; block raw
  terminal control sequences and shell interpolation.
- [ ] Segment by extended graphemes and measure by display cells; never edit or
  truncate by bytes or scalar-value count.
- [ ] Add lifecycle, paste, Unicode, minimum-size, and fatal-error tests before
  relying on visual snapshots.

### P1: interaction quality and load behavior

- [ ] Make event/action/effect types explicit and keep render side-effect free.
- [ ] Use bounded channels for lossless/bursty traffic and latest-value
  coalescing for resize, mouse move, snapshots, and progress.
- [ ] Await actual events and one-shot animation deadlines; remove permanent
  idle ticks.
- [ ] Batch ready actions into one draw with a bounded latency deadline.
- [ ] Derive key help from the binding table and reject duplicates/shadowing.
- [ ] Model selection and focus separately; restore focus after modal close.
- [ ] Define normal, compact, minimal, and too-small layout behavior.
- [ ] Build layout and hit testing from the same `frame.area()` rectangles.
- [ ] Render complete frames through Ratatui and prohibit concurrent terminal
  writes.
- [ ] Add semantic theme tokens with truecolor/256/16/mono mappings and honor
  `NO_COLOR`.
- [ ] Duplicate every color meaning with text, glyph, geometry, or an
  attribute.
- [ ] Add reducer, router, direct-buffer, `TestBackend`, snapshot, property,
  and PTY test layers.

### P2: polish, compatibility, and operations

- [ ] Test supported emulators and platforms for key kinds, Unicode width,
  resize, optional protocols, and cleanup.
- [ ] Provide ASCII-safe glyphs, reduced motion, monochrome rendering, and a
  stable line-oriented/non-fullscreen mode.
- [ ] Validate with actual screen readers and keyboard-only users; do not infer
  accessibility from color contrast or WCAG-inspired behavior alone.
- [ ] Virtualize large collections and cache expensive derived presentation by
  model revision.
- [ ] Add explicit capability/user overrides for color and optional terminal
  protocols.
- [ ] Add structured, redacted tracing to a non-terminal sink and retain its
  flush guard through shutdown.
- [ ] Track queue pressure, coalescing, update time, draw time, task lifecycle,
  and cleanup failures.
- [ ] Run failure injection for partial setup, event errors, worker panic,
  channel closure, draw errors, and cleanup errors.

## Primary-source index

- Ratatui 0.30.2:
  [crate docs](https://docs.rs/ratatui/0.30.2/ratatui/),
  [`Terminal`](https://docs.rs/ratatui/0.30.2/ratatui/struct.Terminal.html),
  [`run`](https://docs.rs/ratatui/0.30.2/ratatui/fn.run.html),
  [event handling](https://ratatui.rs/concepts/event-handling/),
  [application patterns](https://ratatui.rs/concepts/application-patterns/),
  [testing snapshots](https://ratatui.rs/recipes/testing/snapshots/).
- Crossterm 0.28.1:
  [event module](https://docs.rs/crossterm/0.28.1/crossterm/event/index.html),
  [`Event`](https://docs.rs/crossterm/0.28.1/crossterm/event/enum.Event.html),
  [`EventStream`](https://docs.rs/crossterm/0.28.1/crossterm/event/struct.EventStream.html),
  [terminal module](https://docs.rs/crossterm/0.28.1/crossterm/terminal/index.html),
  [style module](https://docs.rs/crossterm/0.28.1/crossterm/style/index.html).
- Tokio 1.53.1:
  [`select!`](https://docs.rs/tokio/1.53.1/tokio/macro.select.html),
  [`spawn_blocking`](https://docs.rs/tokio/1.53.1/tokio/task/fn.spawn_blocking.html),
  [MPSC](https://docs.rs/tokio/1.53.1/tokio/sync/mpsc/index.html),
  [time](https://docs.rs/tokio/1.53.1/tokio/time/index.html),
  [signals](https://docs.rs/tokio/1.53.1/tokio/signal/index.html).
- Unicode:
  [UAX #29, text segmentation](https://unicode.org/reports/tr29/),
  [UAX #11, East Asian Width](https://unicode.org/reports/tr11/),
  [`unicode-segmentation` 1.13.3](https://docs.rs/unicode-segmentation/1.13.3/unicode_segmentation/),
  [`unicode-width` 0.2.2](https://docs.rs/unicode-width/0.2.2/unicode_width/).
- Accessibility and color:
  [WAI-ARIA keyboard interface](https://www.w3.org/WAI/ARIA/apg/practices/keyboard-interface/),
  [modal dialog pattern](https://www.w3.org/WAI/ARIA/apg/patterns/dialog-modal/),
  [WCAG use of color](https://www.w3.org/WAI/WCAG22/Understanding/use-of-color),
  [NO_COLOR](https://no-color.org/).
- Security:
  [MITRE CWE-150](https://cwe.mitre.org/data/definitions/150.html).
