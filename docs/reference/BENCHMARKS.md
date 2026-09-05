# How fast it is

Every number on this page names the harness in this repository that
produced it, with the sample count beside it. Nothing is estimated and
nothing is rounded up from a hope. Where a figure does not exist, this page
says so instead of inventing one. Numbers whose source was a disposable rig,
a shell shim, or a run whose data is not in this repository were removed
rather than kept on trust.

## The transport lanes

Produced by `src/cyclopsd/tests/evidence/release_transport_benchmark.rs`, the opt-in
harness that measures one frozen release candidate. Measured on 2026-08-22
against frozen candidate `c108dea169241f8891e2bfdd3c0ff19280a11c45`. The rig
used an isolated Cyclops home, daemon, tmux server, sender pane, and
recipient pane. It did not launch a vendor CLI or touch the live daemon.
Sampling was serial at a load average of 3.7 on the 18-core machine
described below.

Latency is in milliseconds. `n` is the sample count. CPU/op is client CPU
time per operation. Processes/op counts child processes started by the
client. Byte counts are combined request and response traffic where the
harness measured them.

| Lane | n | p50 | p95 | p99 | CPU/op | Processes/op | Bytes/op |
|---|---:|---:|---:|---:|---:|---:|---:|
| Persistent socket ping | 50 | 0.012 | 0.032 | 0.098 | 0.012 | 0 | 94.6 |
| Persistent socket status | 50 | 0.023 | 0.037 | 0.048 | 0.015 | 0 | 1,037.0 |
| Socket connect and handshake | 50 | 0.019 | 0.029 | 0.063 | 0.016 | 0 | not recorded |
| Cold Cyclops CLI floor | 50 | 2.476 | 2.777 | 3.109 | 2.180 | 1 | not recorded |
| `msg.send` over an open socket | 50 | 5.063 | 17.999 | 30.053 | 0.038 | 0 | 360.6 |
| `cyclops send` CLI | 50 | 10.991 | 12.966 | 14.961 | 2.538 | 1 | not recorded |
| Peer CLI send until visible in inbox | 10 | 16.085 | 28.452 | 28.452 | 4.209 | 1 | 4,979.3 |
| Claim over an open socket | 10 | 10.003 | 15.801 | 15.801 | 0.068 | 0 | 488.1 |
| Peer CLI send through exact claim | 10 | 27.033 | 34.016 | 34.016 | 4.598 | 1 | 1,064.0 |

What each lane includes, and what it does not:

- The socket lanes hold one connection open and time one request. They
  include no process start and no journal write.
- The cold CLI floor starts the `cyclops` binary and connects; it sends
  nothing. Subtracting it from `cyclops send` leaves 8.515ms at p50 for the
  command's remaining work, most of which is the daemon's synchronous
  fsync of the workspace journal.
- `msg.send` and `cyclops send` end when the daemon answers, which is after
  the message is durable and before any doorbell is written. They include
  no terminal write and no receipt.
- The peer lanes run a real `cyclops send` inside the sender pane. "Until
  visible in inbox" ends when the recipient's `inbox.list` shows the
  message; "through exact claim" ends when the recipient's claim returns
  the body. Neither includes a doorbell or a model turn: no vendor CLI ran
  in the isolated panes.
- The p95 and p99 values for the ten-sample lanes are both the
  second-slowest sample and should be read only as a small-sample tail.

The same run resolved the workspace journal before sending and read it in
process immediately after each response. All 30 of 30 messages were present
when `msg.send` returned. None appeared late or remained absent after five
seconds. The measured send response therefore includes the synchronous
durable append rather than a promise to write later.

These figures describe one host and one boot, not a cross-machine
performance guarantee.

**The doorbell itself is not measured by any retained harness.** No row on
this page times a paste, a readback, an Enter, or a receipt. That lane is
first on the queue in [NEXT.md](../development/NEXT.md).

### The deadlines the lanes run under

None of these is an interval; each is a one-shot bounded by a delivery in
flight ([INVARIANTS.md](../development/INVARIANTS.md) rule 8). They are
code constants, not measurements.

| Deadline | Default | What it bounds | Source |
|---|---|---|---|
| `receipt_block_ms` | 2500ms | How long `msg.send` observes an immediately decidable receipt | `src/cyclopsd/src/config.rs`, `Config::defaults` |
| `ack_timeout_ms` | 1500ms | How long a submitted doorbell waits for the hook ACK before screen evidence is accepted | `src/cyclopsd/src/config.rs`, `Config::defaults` |
| `gate_hold_notify_ms` | 120000ms | One admin ping for a doorbell wedged in gating. The hold itself keeps waiting on events | `src/cyclopsd/src/config.rs`, `Config::defaults` |
| `SCREEN_ACK_DEADLINE` | 5s | No receipt by then settles the attempt as `notified` with no verifier | `src/cyclopsd/src/delivery/mod.rs` |
| `ACK_CHECKPOINTS_MS` | 250, 750, 1500, 3000, 5000 | One-shot screen-evidence checks after Enter. Events also wake the waiter; these cap the captures per attempt | `src/cyclopsd/src/delivery/mod.rs` |
| `VERIFY_DELAYS_MS` | 0, 120, 240, 480 | Post-paste readback re-reads, because a repaint can lag a frame | `src/cyclopsd/src/delivery/mod.rs` |
| `DECLINE_SPACING` | 250ms | Spacing between a manifest's modal decline keys | `src/cyclopsd/src/delivery/mod.rs` |
| CLI connect / read | 2s / 5s | The `cyclops` client's own socket budget. The 5s read budget has to exceed `receipt_block_ms`, and does | `src/cyclops/src/client.rs` |
| Workspace `IO_TIMEOUT` | 250ms | The full-screen workspace's budget for its small decoration, naming, and confirmation requests to the daemon | `src/cyclops-workspace/src/daemon.rs` |
| `WAIT_DEFAULT_MS` / `WAIT_MAX_MS` | 60s / 600s | `agent.wait` | `src/cyclopsd/src/delivery/mod.rs` |

## Machine context for the workspace and stream numbers

Everything labeled "measured here" below ran on this box on 2026-08-08, in
the `release` profile.

```
$ sysctl -n machdep.cpu.brand_string
Apple M5 Pro
$ sysctl -n hw.ncpu
18
$ sw_vers -productVersion
26.5.2
$ tmux -V
tmux 3.6a
$ rustc --version
rustc 1.97.1 (8bab26f4f 2026-07-14) (Homebrew)
```

Read every wall-clock figure with two caveats. The working tree carried
uncommitted edits under `src/cyclops-workspace` while these ran, so the
commit alone does not reproduce the tree byte for byte. And the box was
compiling in another checkout at the same time. Trust shapes over absolute
milliseconds, and rerun anything that matters.

## Throughput

Three different things get called throughput here. They are not
interchangeable.

**Pane bytes.** `baseline_pane_runtime_feed_and_grid_throughput` in
`src/cyclops-workspace/tests/baseline.rs` feeds 1 MiB of mixed ASCII, SGR
color, and CJK wide characters through a `PaneRuntime` in 4096-byte chunks,
one frame per chunk (one run per pane size). Measured here:

| Pane size | Feed throughput | Grid build (test path) | Direct cell walk (production path) |
|---|---|---|---|
| 80x24 | 145.2 MB/s | 9.77us/frame | 7.74us/frame |
| 200x50 | 83.1 MB/s | 52.24us/frame | 45.06us/frame |

**Sustained output under a real tmux.**
`sustained_output_backlog_drains_continuously` in
`src/cyclops-workspace/tests/perf_contract.rs`, measured here, one run:
7,000,074 bytes drained in 93 batches at the 8ms render cadence, peak batch
87,388 bytes, longest run of empty cycles while the stream was active: 5.

**Stream events.** `frame_build_stays_under_16ms_at_10k_entries` in
`src/cyclops-ui/tests/perf.rs` fills the event ring to its 10,000-entry cap,
adds a real attention backlog, and builds one frame at 80x60 (one frame per
row). The test asserts a 16ms budget, one 60Hz frame. Measured here:

| Open attention items | Firehose view | Admin view |
|---|---|---|
| 0 | 0.042ms | 0.100ms |
| 100 | 0.117ms | 0.180ms |
| 400 | 0.138ms | 0.268ms |
| 1000 | 0.164ms | 0.285ms |

The worst case is 0.285ms against a 16ms budget, at 10,000 entries with a
thousand open items.

**Message throughput is not measured by any retained harness.**
`src/cyclopsd/tests/evidence/concurrent_messaging_perf.rs` measures durable
acceptance under four concurrent callers (three samples of 4 x 32 messages
to the administrator mailbox, which has no pane route) and keeps the
per-message timings as scheduled-lane evidence, not as a rate. No saturation
test exists. Do not quote a deliveries-per-second number from this repo.

## The workspace

### Render

`full_frame_paint_duration_scales_with_pane_count` in
`src/cyclops-workspace/src/render/canvas.rs` paints complete frames, tab bar
plus every pane, into a real Ratatui buffer over 200 iterations. Each pane
is 30 columns by 48 rows holding 8 KB of mixed content, so the 8-pane canvas
is 256x51. Two runs on this box, n=200 each:

| Panes | Canvas | Median, run 1 | Median, run 2 | p90, run 2 |
|---|---|---|---|---|
| 1 | 32x51 | 62.5us | 63.6us | 87.0us |
| 4 | 128x51 | 274.1us | 253.6us | 344.2us |
| 8 | 256x51 | 555.5us | 543.4us | 674.1us |

The test records; it asserts no wall-clock budget. That is deliberate. The
module doc on `src/cyclops-workspace/tests/baseline.rs` names the reason:
`src/cyclops-ui/tests/perf.rs` does assert a wall-clock budget and has
flaked under parallel CPU load. F45 in [findings.md](../../findings.md)
generalizes it, listing three distinct ways a starved runner invalidates a
timing test's premise, which is why the perf-contract tests check
signatures instead of clocks.

For scale, `RENDER_DEBOUNCE` in `src/cyclops-workspace/src/app.rs` is 8ms,
so an 8-pane frame at 543us uses about 7% of the window it has.

### Input

`key_to_control_write_latency` and
`key_to_control_write_latency_during_output_flood` in
`src/cyclops-workspace/tests/perf_contract.rs`, measured here, n=500 each:

| Pane condition | p50 | p95 | Max |
|---|---|---|---|
| Idle | 0.5us | 0.8us | 4.9us |
| Flooding (7,600,768 bytes delivered during the sampling loop) | 11.4us | 18.8us | 68.7us |

A flooding pane costs roughly 23x the median of an idle one and stays under
70us at the observed maximum.

### Reconciliation and hydration

`src/cyclops-workspace/tests/baseline.rs` keeps the pre-refactor shapes
alongside the current ones so the comparison survives
(`baseline_reconciliation_fan_out`, `baseline_reconciliation_workspace_snapshot`,
`baseline_hydration_latency_serial`, `baseline_hydration_latency_concurrent`;
one run per row). Measured here:

| Windows | Old fan-out (`list-sessions` + membership + `list-windows` + `list-panes` per window) | Current `workspace_snapshot` |
|---|---|---|
| 1 | 13.69ms, 4 commands (W+3) | 0.27ms, 3 commands (fixed) |
| 4 | 22.93ms, 7 commands (W+3) | 0.38ms, 3 commands (fixed) |
| 8 | 36.22ms, 11 commands (W+3) | 0.45ms, 3 commands (fixed) |

| Panes | Old serial `hydrate_pane` loop | Current concurrent `hydrate_panes` |
|---|---|---|
| 1 | 0.17ms | 0.28ms |
| 4 | 0.57ms | 0.30ms |
| 8 | 1.68ms | 0.38ms |

At eight windows the snapshot is about 80x faster in wall time and the
command count stops scaling with window count at all. At one pane the
concurrent hydration is slightly slower than serial, which is the expected
floor: there is nothing to overlap with a single pane.

### Coalescing and flow control

Measured here, from `src/cyclops-workspace/tests/perf_contract.rs`
(`decoration_burst_coalesces_into_one_refresh`,
`decoration_stream_refreshes_repeatedly_during_the_stream`,
`flow_control_pause_and_resume`; one run each):

- 100 decoration signals sent in 0.001ms produced exactly one refresh,
  35.03ms after the first signal. `DECORATION_DEBOUNCE` is 30ms and is armed
  once by the first event in a burst, never pushed back by a later one.
- 34 signals spread over 200ms produced 5 refreshes at 30.7, 66.8, 103.3,
  138.2 and 174.5ms, gaps of 34.9 to 36.4ms. The deadline arms once per
  burst and fires; it does not slide.
- Flow control round trip: pause to confirmed continue 0.11ms, continue to
  rehydrate complete 0.16ms.

## Cost

### Processes

A Cyclops send spawns no tmux process. `cyclopsd` holds one long-lived
`tmux -u -L <socket> -f /dev/null -C attach-session` per watched session
(`src/cyclops-tmux/src/control.rs`), and the delivery path writes through it:
`TmuxInjector` in `src/cyclopsd/src/delivery/inject.rs` calls
`load_buffer`, `paste_buffer`, `send_keys`, and `capture_pane` on the
`ControlClient`, never a subprocess. The Processes/op column in the
transport table above counts the same thing from the client side: the one
process a CLI lane starts is the `cyclops` binary itself.

### Idle

`src/cyclopsd/tests/evidence/idle_observation_perf.rs` first proves, on one isolated
control fixture, that every application counter moves after visible output.
It then starts a fresh isolated screen-tier fixture, resets its counters
after attachment, and observes a fixed quiet window, counting watcher
events, state-observation recompute starts, and state-observation
`capture-pane` requests. A retained zero is therefore not a vacuous
uninstrumented result. It does not count operating-system scheduler wakeups,
tmux internals, client refreshes, or terminal-delivery captures, so it is
not a replacement for a CPU or battery measurement. It runs in the scheduled
and release lanes; this page quotes no number from it.

`src/cyclopsd/tests/evidence/cold_start_replay_perf.rs` boots a fresh in-process
daemon over growing workspace journals and proves the replayed body-free
snapshot still sees every seeded message. It measures in-process boot, not
executable launch, client connection, or a doorbell. It also runs in the
scheduled lanes and is quoted nowhere on this page.

## What is not fast, and what is not measured

**Pane contrast re-grounding is the known open item.** `matched_ground` and
`readable_fg` in `src/cyclops-workspace/src/render/mod.rs` run per cell on
every pane frame with colors on: a luminance and WCAG contrast computation
per cell, plus a color emit. The operator reports this path as noticeably
expensive against a truecolor ground. **No committed test isolates that
cost.** The full-frame paint test above builds its theme with
`Paint::for_test()`, which is `Theme::default()` with `truecolor: false`,
so its 543us at eight panes measures the 256-color path, not the truecolor
one. Treat it as an operator report awaiting a benchmark.

**Resize on a runtime holding scrollback is the most expensive single
workspace operation measured.** 50 alternating 80x24 to 120x40 resizes on a
runtime holding 2000 lines of history: 22.91ms total, 457.74us average,
1045us maximum, measured here by `baseline_resize_cost_with_scrollback` in
`src/cyclops-workspace/tests/baseline.rs` (n=50). One average resize costs
about 0.84x an entire 8-pane frame paint, and the worst one costs 1.9x.
Resize coalescing (at most one tmux resize per render deadline) exists to
stop paying that once per mouse-move during a drag.

**Not yet measured by a retained, comparable workload:**

- The doorbell lane: paste, readback, Enter, and receipt against a real
  vendor CLI.
- Operating-system scheduler wakeups, idle CPU time, battery use, and memory
  growth over a long session. The quiet-pane workload counts only Cyclops
  application-level observation work during one bounded window.
- Terminal delivery under concurrency. The retained concurrent workload covers
  durable mailbox acceptance, not route selection, notification, or injection.
- Comparable Linux and macOS timing. The scheduled runner currently retains
  performance artifacts on Linux; the macOS matrix is correctness evidence.

**Known slow by design, and correctly so:** a doorbell held in gating waits
as long as a human draft is visible in the composer, a modal needs a person,
or a quota screen is up. Those are unbounded on purpose. The only clock on
them is `gate_hold_notify_ms`, and all it does is tell the admin the hold
exists.

## Reproducing every number

Machine context:

```bash
sysctl -n machdep.cpu.brand_string; sysctl -n hw.ncpu
sw_vers -productVersion; tmux -V; rustc --version
```

The transport lanes, against a frozen candidate pair built from a clean
checkout (the harness refuses a dirty tree or a mismatched version):

```bash
CYC_RELEASE_SHA=<sha> CYC_RELEASE_CYCLOPS=<path> CYC_RELEASE_CYCLOPSD=<path> \
  cargo test --release -p cyclopsd --test evidence -- --ignored frozen_candidate
```

Render, throughput, hydration, reconciliation, resize:

```bash
CARGO_INCREMENTAL=0 cargo test --release -p cyclops-workspace \
  --test baseline -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 cargo test --release -p cyclops-workspace \
  --test perf_contract -- --nocapture
CARGO_INCREMENTAL=0 cargo test --release -p cyclops-workspace \
  --lib render::canvas::tests::full_frame_paint_duration_scales_with_pane_count \
  -- --nocapture
CARGO_INCREMENTAL=0 cargo test --release -p cyclops-ui --test perf -- --nocapture
```

Every rig uses its own tmux server and its own home directory, and tears
both down afterwards. Never point one at a live session.
