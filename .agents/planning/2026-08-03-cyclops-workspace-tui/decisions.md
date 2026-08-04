# Implementation decisions and deviations

Running log for the workspace TUI build. Each step adds its decisions;
deviations from the detailed design are called out explicitly.

## Step 1 — VT engine evaluation

**Decision:** `alacritty_terminal` 0.26 is the production pane engine,
called directly via `AlacrittyVt` (no trait).

**Deviation:** The design named `libghostty-vt` as the second corpus
implementor. It cannot build without Zig on PATH (F34), so the comparison
engine is `vt100` 0.16 (dev-dependency, tests only).

**Deviation:** The design kept a `PaneVt` trait while two engines were in
play. After the corpus (`alacritty` 12/12, `vt100` 5/12), the trait was
collapsed per the design's own rule (F35).

## Step 2 — Streaming control client

**Decision:** No adapter changes required. `ControlClient::spawn` already
exposes a blocking byte-line reader, typed `Notification` fan-out, octal
unescaping (`notify.rs` unit tests), and clean `shutdown()`.

**Added:** `crates/cyclops-tmux/tests/streaming_client.rs` pins the workspace
plan's acceptance criteria (decoded echo bytes, structural notifications from
rig `cmd()` mutations).

**Probe:** `cargo test -p cyclops-tmux streaming_client -- --nocapture`

## Step 3 — Hydration bundles and PaneRuntime

**Added:** `cyclops-tmux::HydrationBundle` with `hydrate_pane`,
`set_client_size`, `set_window_size_latest`, and alternate-screen capture.
`cyclops-workspace::PaneRuntime` wraps `AlacrittyVt`.

**Deviation:** Mid-stream rehydrate during active scrolling compares content
presence rather than row-for-row equality when the viewport is moving.

**Probe:** `cargo test -p cyclops-workspace --test hydration`

## Step 4 — Minimal workspace

**Added:** Ratatui 0.30 + Crossterm 0.28 workspace TUI; bare `cyclops` on a
TTY dispatches into `cyclops_workspace::run()`. Prefix `C-b d` detaches.

**Probe:** `cargo test -p cyclops bare_non_tty`; manual demo on a tty with tmux.

**Note:** `DETACHED` prints on stderr after terminal restore when prefix-`d` detaches.

