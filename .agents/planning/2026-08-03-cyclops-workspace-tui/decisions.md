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
