# How fast it is

Every number on this page was produced by a harness in this repository, run
on one machine on 2026-09-05, and every row names that harness and its
sample count. Nothing here is estimated. Where a figure does not exist,
this page says so instead of inventing one.

This is a before-and-after page. It compares the 1.0.2 tree against the
1.1.0 tree that the lean-core and UI-speed work merged to `main`. Most lanes
did not move. The page says so plainly where that is the answer.

## What was compared

| Side | Commit | Version | Where it ran |
|---|---|---|---|
| Before | `d3fca734a463d06d842de845fb3878b43b364069` | 1.0.2 | a detached worktree at that commit |
| After | `48d67a083b3f8ccb53f15d34e96f996e39765cc3` | 1.1.0 | a worktree whose tree is byte-identical to that merge |

The machine:

```
$ sysctl -n machdep.cpu.brand_string
Apple M5 Pro
$ sysctl -n hw.ncpu
18
$ sysctl -n hw.memsize
68719476736
$ sw_vers -productVersion
26.5.2
$ tmux -V
tmux 3.6a
$ rustc --version
rustc 1.97.1 (8bab26f4f 2026-07-14) (Homebrew)
```

Method. Every lane ran three times per side in the `release` profile. The
two sides were interleaved run by run, so drift in background load lands on
both. Each cell below is the median of those three runs. A lane whose three
runs spread more than 20 percent from smallest to largest is marked with a
dagger, and a spread that wide means the two sides cannot be told apart.
The one-minute load average at the start of each run ranged from 4.2 to 8.0
on this 18-core box, because another build shared the machine. Read shapes,
not absolute milliseconds, and rerun anything that matters.

## Messaging transport

`src/cyclopsd/tests/evidence/communication_benchmark.rs`, 50 operations per
lane per run. The rig owns an isolated Cyclops home, daemon, and tmux server.
No vendor CLI ran in its panes. Latency is in milliseconds.

| Lane | n | 1.0.2 p50 | 1.1.0 p50 | change | 1.0.2 p95 | 1.1.0 p95 |
|---|---:|---:|---:|---:|---:|---:|
| Raw tmux `send-keys` | 50 | 2.564 | 2.611 | +1.8% | 2.798 | 3.034 |
| Raw tmux `capture-pane` | 50 | 2.598 | 2.504 | -3.6% | 2.859 | 2.600 |
| smux tmux-bridge cycle (modeled) | 50 | 7.996 | 7.652 | -4.3% | 8.405 | 8.259 |
| Cyclops `msg.send` (durable append) | 50 | 3.025 | 3.059 | +1.1% | 4.075 | 4.043 |
| Cyclops `inbox.claim` (body access) | 50 | 15.946 | 15.118 | -5.2% | 21.836 | 17.442 |

No median in this table moved. Every p50 difference is smaller than the run
to run spread on the raw tmux lanes, which are the control: those lanes
contain no Cyclops code at all and still shift by a few percent between runs.

One tail did move. The three `inbox.claim` p95 samples were 21.8, 22.0, and
21.0 ms on 1.0.2 against 17.4, 17.2, and 18.0 ms on 1.1.0, and the two sets
do not overlap. The p99 separates the same way, 22.1 to 24.1 ms against 19.0
to 19.9 ms. The claim median is unchanged, so what got cheaper is the slow
end of a claim, not the ordinary one.

Resource profile from the same runs:

| Process | 1.0.2 RSS | 1.1.0 RSS | 1.0.2 open FDs | 1.1.0 open FDs |
|---|---:|---:|---:|---:|
| tmux server | 4.08 MB | 3.97 MB | 16 | 16 |
| `cyclopsd` (in-process) | 11.03 MB | 11.11 MB | 28 | 28 |

The workspace journal after 50 sends and 50 claims was 63 KB on 1.0.2 and
60 KB on 1.1.0.

### What the tmux and smux comparison is worth

Read the harness before quoting these rows against another project.

- **Raw tmux `send-keys`** is one real `tmux send-keys -t <pane> <text>
  Enter` against the rig's own server. It starts one tmux client process and
  proves nothing: no durable record, no sender identity, no ordering, no
  claim, no receipt.
- **Raw tmux `capture-pane`** is one real `tmux capture-pane -p -t <pane>`,
  again one process.
- **The smux row is modeled, not measured against smux.** No ShawnPana/smux
  code runs. The harness reconstructs that project's documented tmux-bridge
  messaging cycle out of real tmux calls: write a read-guard file, run
  `capture-pane -p -e -S -50`, run `send-keys -l` with the literal text, run
  `send-keys Enter`, then remove the guard file. What the row measures is the
  cost of that shape on this machine, which is dominated by three tmux client
  processes. Treat it as the price of the protocol, never as a benchmark of
  the smux implementation.
- **`msg.send`** is called in process on the daemon. It starts no process and
  makes no socket round trip. It returns once the message and one mailbox
  entry per recipient are appended to the workspace journal and fsynced.
- **`inbox.claim`** is a real request over the rig's daemon socket.

So the tmux and smux lanes each pay one or more process starts and the
Cyclops send lane pays none, while the Cyclops send lane pays an fsync the
others never pay. The rows are not a like-for-like race. They are the cost
of four different contracts.

### The lane this page no longer quotes

`src/cyclopsd/tests/evidence/release_transport_benchmark.rs` measures a
frozen release candidate over the real CLI and the real socket, including a
cold `cyclops` process start. It refuses to run without `CYC_RELEASE_SHA`
matching a clean checkout and prebuilt candidate binaries. It was not run for
this page, so the cold-CLI and peer-pane rows that used to sit here are gone
rather than carried forward from a 1.0.x candidate.

## Concurrent mailbox acceptance

`src/cyclopsd/tests/evidence/concurrent_messaging_perf.rs`. Three samples of
four concurrent callers sending 32 messages each to the administrator
mailbox, which has no pane route. That is 384 timed requests per run.

| Measure | n | 1.0.2 | 1.1.0 | change |
|---|---:|---:|---:|---:|
| Request latency p50 | 384 | 13.074 ms | 12.963 ms | -0.8% |
| Request latency p95 | 384 | 24.055 ms | 23.960 ms | -0.4% |
| Request latency max † | 384 | 49.306 ms | 53.122 ms | inside the spread |
| One 128-message sample, wall clock | 3 | 447.2 ms | 441.8 ms | -1.2% |

Both sides kept strictly increasing durable sequence numbers per caller. This
lane measures durable acceptance under contention. It is not a saturation
test and it is not a deliveries-per-second rate. Do not quote one from it.

## Daemon cold start and replay

`src/cyclopsd/tests/evidence/cold_start_replay_perf.rs`. It boots a fresh
in-process daemon over a workspace journal of a given size, three boots per
size per run, and proves the replayed body-free snapshot still sees every
seeded message. It measures in-process boot only: no executable launch, no
client connection, no doorbell.

| Journal | boots | 1.0.2 p50 | 1.1.0 p50 | change |
|---|---:|---:|---:|---:|
| 0 messages | 3 per run | 45.8 ms | 51.1 ms | +11.5% |
| 1,000 messages | 3 per run | 60.1 ms | 65.5 ms | +8.9% |
| 10,000 messages | 3 per run | 92.5 ms | 95.9 ms | +3.6% |

**1.1.0 boots slower, and this is the one lane where it does.** The slower
side won eight of the nine paired runs behind this table. The gap is about
5 ms at an empty journal, about 5 ms at a thousand lines, and about 3 ms at
ten thousand lines. The absolute cost does not grow with the journal, so the extra work is
in fixed startup, not in replay. Per-line replay did not get more expensive.
This page does not claim to know which piece of startup grew; no harness here
isolates that.

The journal itself got slightly smaller: 798,783 bytes for 1,000 messages on
1.0.2 against 772,783 bytes on 1.1.0, and 8,007,784 against 7,747,784 bytes
at 10,000.

## Idle observation

`src/cyclopsd/tests/evidence/idle_observation_perf.rs`. It first proves on a
separate control fixture that one line of output moves every application
counter. It then observes a fresh quiet screen-tier pane for a 1,000 ms
window with the counters reset.

| Counter | 1.0.2 | 1.1.0 |
|---|---:|---:|
| Watcher event wakes | 0 | 0 |
| Observation recompute starts | 0 | 0 |
| State-observation `capture-pane` requests | 0 | 0 |

Three runs per side, all zero, with the positive control at 1 each time. This
counts Cyclops application work only. It does not count operating-system
scheduler wakeups, tmux internals, client refreshes, or terminal-delivery
captures, so it is not a CPU or battery measurement.

## Stream events

`frame_build_stays_under_16ms_at_10k_entries` in
`src/cyclops-ui/tests/perf.rs`. The event ring is filled to its 10,000-entry
cap, a real attention backlog is added, and one frame is built at 80x60. The
test records the median frame build and asserts a 16 ms budget, which is one
60 Hz frame.

| Open attention items | 1.0.2 firehose | 1.1.0 firehose | 1.0.2 admin | 1.1.0 admin |
|---|---:|---:|---:|---:|
| 0 † | 0.061 ms | 0.056 ms | 0.034 ms | 0.033 ms |
| 100 † | 0.235 ms | 0.269 ms | 0.115 ms | 0.122 ms |
| 400 † | 0.285 ms | 0.311 ms | 0.184 ms | 0.189 ms |
| 1000 † | 0.357 ms | 0.335 ms | 0.352 ms | 0.352 ms |

Every row spread more than 20 percent across its three runs, so no row here
separates the two sides. The honest summary is the one both sides share:
every median is under 0.4 ms against a 16 ms budget, and the slowest single
run of any row was 0.43 ms, at 10,000 entries with a thousand open items.

## Throughput

Two different things get called throughput. They are not interchangeable.

**Pane bytes.** `baseline_pane_runtime_feed_and_grid_throughput` in
`src/cyclops-workspace/tests/baseline.rs` feeds 1 MiB of mixed ASCII, SGR
color, and CJK wide characters through a `PaneRuntime` in 4096-byte chunks,
one frame per chunk, one run per pane size.

| Pane size | Measure | 1.0.2 | 1.1.0 | change |
|---|---|---:|---:|---:|
| 80x24 | Feed throughput | 179.7 MB/s | 178.5 MB/s | -0.7% |
| 80x24 | Grid build (test path) | 8.24 us/frame | 8.19 us/frame | -0.6% |
| 80x24 | Direct cell walk (production path) | 7.14 us/frame | 7.12 us/frame | -0.3% |
| 200x50 | Feed throughput | 102.0 MB/s | 99.0 MB/s | -2.9% |
| 200x50 | Grid build (test path) | 43.00 us/frame | 43.02 us/frame | +0.0% |
| 200x50 | Direct cell walk (production path) | 36.76 us/frame | 36.78 us/frame | +0.1% |

Nothing moved.

**Sustained output under a real tmux.**
`sustained_output_backlog_drains_continuously` in
`src/cyclops-workspace/tests/perf_contract.rs`, one run per rep.

| Measure | 1.0.2 | 1.1.0 |
|---|---:|---:|
| Bytes drained | 7,000,135 | 7,000,116 |
| Drain batches at the 8 ms cadence | 48 | 48 |
| Batches carrying data | 48 | 48 |
| Peak batch | 154,364 bytes | 153,657 bytes |
| Longest empty-cycle run while the stream was active | 0 | 0 |
| Continuity gaps | 0 | 0 |

One 1.0.2 run of the three was starved by the shared machine and reported 23
continuity gaps over 6,435,675 bytes. The other five runs across both sides
reported none. That outlier is a fact about this box, not about 1.0.2.

**Message throughput is still not measured by any retained harness.** The
concurrent lane above measures durable acceptance, not a rate. No saturation
test exists. Do not quote a deliveries-per-second number from this repository.

## The workspace

### Render

`full_frame_paint_duration_scales_with_pane_count` in
`src/cyclops-workspace/src/render/canvas.rs` paints complete frames, tab bar
plus every pane, into a real Ratatui buffer over 200 iterations per pane
count. Each pane is 30 columns by 48 rows holding 8 KB of mixed content.

| Panes | Canvas | 1.0.2 median † | 1.1.0 median † | 1.0.2 p90 † | 1.1.0 p90 † |
|---|---|---:|---:|---:|---:|
| 1 | 32x51 | 82.4 us | 110.0 us | 138.9 us | 137.9 us |
| 4 | 128x51 | 268.5 us | 296.4 us | 300.5 us | 374.7 us |
| 8 | 256x51 | 505.0 us | 503.3 us | 521.6 us | 517.1 us |

Every row spread more than 20 percent, and the third run was slow on both
sides at once, so this table separates nothing. Paired run by run the two
sides track each other: the 8-pane medians were 505.0 against 502.5, 505.0
against 503.3, and 842.5 against 884.2. Frame composition did not change.

The test records; it asserts no wall-clock budget. That is deliberate. The
module doc on `src/cyclops-workspace/tests/baseline.rs` names the reason:
`src/cyclops-ui/tests/perf.rs` does assert a wall-clock budget and has flaked
under parallel CPU load, and a tight timing assertion on a shared machine is
a bug waiting for a busy runner.

`RENDER_DEBOUNCE` in `src/cyclops-workspace/src/app.rs` is 8 ms, so an
8-pane frame at 503 us uses about 6 percent of the window it has.

### Messages pane frame time

`messages_pane_frame_time_with_100_messages` in
`src/cyclops-workspace/src/app.rs` draws 200 frames of an 80x40 workspace
with the Messages pane open on 100 rows and reports the mean frame time. It
is `#[ignore]`d and opt-in.

| Tree | Mean frame time | Source |
|---|---:|---|
| Pre-#229 | 1.94 ms | measured by the author of that change with this test, on the tree before it |
| 1.1.0 † | 0.084 ms | measured here, median of three runs |

**This test does not exist on 1.0.2**, so the before value is quoted from its
author rather than re-measured. The 1.1.0 runs were 0.084, 0.083, and 0.119
ms. Even the slowest is far under the earlier figure. This is the one lane on
this page where 1.1.0 is decisively faster.

### Input

`key_to_control_write_latency` and
`key_to_control_write_latency_during_output_flood` in
`src/cyclops-workspace/tests/perf_contract.rs`, n=500 per run.

| Pane condition | Measure | 1.0.2 | 1.1.0 |
|---|---|---:|---:|
| Idle | p50 | 0.7 us | 0.7 us |
| Idle | p95 | 1.2 us | 1.7 us |
| Idle | max † | 464.1 us | 488.2 us |
| Flooding | p50 † | 19.2 us | 14.5 us |
| Flooding | p95 † | 33.8 us | 33.5 us |
| Flooding | max † | 219.3 us | 149.4 us |

About 6.1 MB reached the pane during each flooding sampling loop on both
sides. The medians say a flooding pane costs 20 to 30 times an idle one, and
the flooding rows spread too far to separate the two sides. The maxima are
scheduler noise on a shared machine and should not be read as a tail
guarantee.

### Reconciliation and hydration

`src/cyclops-workspace/tests/baseline.rs` keeps the pre-refactor shapes
beside the current ones (`baseline_reconciliation_fan_out`,
`baseline_reconciliation_workspace_snapshot`,
`baseline_hydration_latency_serial`,
`baseline_hydration_latency_concurrent`), one run per row per rep.

| Windows | Old fan-out, 1.0.2 † | Old fan-out, 1.1.0 † | Snapshot, 1.0.2 † | Snapshot, 1.1.0 |
|---|---:|---:|---:|---:|
| 1 | 12.33 ms, 4 commands | 12.49 ms, 4 commands | 0.18 ms, 3 commands | 0.16 ms, 3 commands |
| 4 | 20.11 ms, 7 commands | 21.44 ms, 7 commands | 0.26 ms, 3 commands | 0.28 ms, 3 commands |
| 8 | 29.49 ms, 11 commands | 35.36 ms, 11 commands | 0.36 ms, 3 commands | 0.35 ms, 3 commands |

| Panes | Serial loop, 1.0.2 | Serial loop, 1.1.0 | Concurrent, 1.0.2 † | Concurrent, 1.1.0 † |
|---|---:|---:|---:|---:|
| 1 | 0.14 ms | 0.15 ms | 0.16 ms | 0.15 ms |
| 4 | 0.52 ms | 0.48 ms | 0.29 ms | 0.30 ms |
| 8 | 1.01 ms | 0.95 ms | 0.40 ms | 0.41 ms |

The old fan-out issues W+3 one-shot tmux processes; the snapshot issues three
whatever the window count. Both sides show the same shape, because the
snapshot landed before 1.0.2. At one pane the concurrent hydration is no
faster than serial, which is the expected floor: with a single pane there is
nothing to overlap.

### Resize with scrollback

`baseline_resize_cost_with_scrollback` in the same file, 50 alternating
80x24 to 120x40 resizes on a runtime holding 2,000 lines of history.

| Measure | n | 1.0.2 | 1.1.0 | change |
|---|---:|---:|---:|---:|
| Total | 50 | 17.95 ms | 18.53 ms | +3.2% |
| Average per resize | 50 | 358.6 us | 370.0 us | +3.2% |
| Worst resize | 50 | 781 us | 817 us | +4.6% |

This is still the most expensive single workspace operation measured here.
One average resize costs about 0.7 times an entire 8-pane frame paint. Resize
coalescing, at most one tmux resize per render deadline, exists so a drag
does not pay that per mouse move.

### Decoration coalescing

`decoration_burst_coalesces_into_one_refresh` and
`decoration_stream_refreshes_repeatedly_during_the_stream` in
`src/cyclops-workspace/tests/perf_contract.rs`, one run each per rep.

| Measure | 1.0.2 | 1.1.0 |
|---|---:|---:|
| 100 signals sent in | 0.002 ms | 0.003 ms |
| Refreshes that burst produced | 1 | 1 |
| First signal to that refresh | 35.04 ms | 34.63 ms |
| Signals spread over 200 ms | 33 | 34 |
| Refreshes they produced | 6 | 5 |

`DECORATION_DEBOUNCE` in `src/cyclops-workspace/src/app.rs` is 30 ms. It is
armed once by the first event in a burst and never pushed back by a later
one, which is why a burst yields one refresh and a stream yields a refresh
about every 35 ms.

`flow_control_pause_and_resume` in the same file is `#[ignore]`d as an opt-in
live tmux soak. It did not run for this page, so the pause and resume figures
this page used to carry are gone.

## Build and install

The installer downloads a published release pair for the host when one
exists, and builds from source only when it does not. The numbers below are
the fallback path, not the normal one.

`scripts/install.sh` builds exactly `cargo build --profile dist -p cyclops -p
cyclopsd`. The `dist` profile is `release` without the thin-LTO link step.
Each measurement below deleted only that profile's artifacts first, so every
run is a clean dist build of the full dependency tree, three per side,
interleaved the same way the lanes above were. The load average at the start
of these six builds ranged from 5.6 to 10.9.

| Measure | n | 1.0.2 | 1.1.0 | change |
|---|---:|---:|---:|---:|
| Clean `dist` build, wall clock | 3 | 30.73 s | 27.00 s | -12.1% |
| `cyclops` binary | 3 | 15,267,776 bytes | 15,811,984 bytes | +3.6% |
| `cyclopsd` binary | 3 | 12,946,592 bytes | 11,457,072 bytes | -11.5% |
| Both binaries | 3 | 28,214,368 bytes | 27,269,056 bytes | -3.4% |

Each build compiled 144 crates from nothing. The three 1.0.2 runs took
30.73, 28.61, and 34.17 seconds; the three 1.1.0 runs took 26.00, 27.00, and
28.08. The binary sizes were byte-identical across the three runs on each
side. `cyclopsd` shrank and `cyclops` grew slightly, and the pair is smaller
overall.

## What each lane includes, and what it does not

Delivery is a durable mailbox plus one doorbell line
([DELIVERY.md](../development/DELIVERY.md) is the spec).

- **`msg.send` ends before any doorbell exists.** It appends the message and
  one mailbox entry per recipient to the workspace journal and fsyncs before
  answering. The response proves durable acceptance and nothing else. No
  terminal write, no Enter, no receipt is inside that number.
- **The socket lanes hold one connection open and time one request.** They
  include no process start.
- **The raw tmux and smux lanes each start tmux client processes.** The
  Cyclops send lane starts none and pays an fsync instead.
- **No lane on this page runs a vendor CLI.** The isolated panes run `cat`
  under a test manifest.
- **The doorbell itself is not measured by any retained harness.** Nothing
  here times a paste, a readback, an Enter, or a receipt. That lane is item 7
  in [NEXT.md](../development/NEXT.md).

### The deadlines the lanes run under

None of these is an interval. Each is a one-shot bounded by a delivery in
flight ([INVARIANTS.md](../development/INVARIANTS.md) rule 8). They are code
constants, not measurements, and each was read from the source below at
`48d67a08`.

| Deadline | Default | What it bounds | Source |
|---|---|---|---|
| `receipt_block_ms` | 2500ms | Cap on observing the first durable disposition of a head whose cached pane verdict lets the worker decide immediately | `src/cyclopsd/src/config.rs`, `Config::defaults` |
| `ack_timeout_ms` | 1500ms | How long a submitted doorbell waits for the hook ACK before screen evidence is accepted | `src/cyclopsd/src/config.rs`, `Config::defaults` |
| `delivery_retry_max` | 1 | Redelivery attempts after the first failure. Never a loop | `src/cyclopsd/src/config.rs`, `Config::defaults` |
| `gate_hold_notify_ms` | 120000ms | One admin ping for a doorbell wedged in gating. The hold itself keeps waiting on events | `src/cyclopsd/src/config.rs`, `Config::defaults` |
| `SCREEN_ACK_DEADLINE` | 5s | No receipt by then settles the attempt as `notified` with no verifier | `src/cyclopsd/src/delivery/mod.rs` |
| `ACK_CHECKPOINTS_MS` | 250, 750, 1500, 3000, 5000 | One-shot screen-evidence checks after Enter. Events also wake the waiter; these cap the captures per attempt | `src/cyclopsd/src/delivery/mod.rs` |
| `VERIFY_DELAYS_MS` | 0, 120, 240, 480 | Post-paste readback re-reads, because a repaint can lag a frame | `src/cyclopsd/src/delivery/mod.rs` |
| `DECLINE_SPACING` | 250ms | Spacing between a manifest's modal decline keys | `src/cyclopsd/src/delivery/mod.rs` |
| `WAIT_DEFAULT_MS` / `WAIT_MAX_MS` | 60s / 600s | `agent.wait` | `src/cyclopsd/src/delivery/mod.rs` |
| `DEFAULT_CONNECT_TIMEOUT` / `DEFAULT_READ_TIMEOUT` | 2s / 5s | The official client's socket budget. The 5s read budget has to exceed `receipt_block_ms`, and does | `src/cyclops-client/src/lib.rs` |
| `IO_TIMEOUT` | 250ms | The full-screen workspace's budget for its small decoration, naming, and confirmation requests to the daemon | `src/cyclops-workspace/src/daemon.rs` |

The CLI reaches those client timeouts through `src/cyclops/src/client.rs`,
which is now a re-export of `cyclops_client` and holds no constants of its
own.

## Cost

### Processes

A Cyclops send spawns no tmux process. `cyclopsd` holds one long-lived
`tmux -C attach-session` per watched session
(`src/cyclops-tmux/src/control.rs`), and the delivery path writes through it:
`TmuxInjector` in `src/cyclopsd/src/delivery/inject.rs` calls `load_buffer`,
`paste_buffer`, `send_keys`, and `capture_pane_joined_escaped` on the
`ControlClient`, never a subprocess. The raw tmux and smux rows in the
transport table are the other side of that: each of their calls is a fresh
tmux client process.

## What is not fast, and what is not measured

**Pane contrast re-grounding is the known open item.** `matched_ground` and
`readable_fg` in `src/cyclops-workspace/src/render/mod.rs` run per cell on
every pane frame with colors on: a luminance and WCAG contrast computation
per cell, plus a color emit. The operator reports this path as noticeably
expensive against a truecolor ground. **No committed test isolates that
cost.** The full-frame paint test above builds its theme with
`Paint::for_test()`, which `src/cyclops-workspace/src/theme.rs` defines with
`truecolor: false`, so its 503 us at eight panes measures the 256-color path,
not the truecolor one. Treat the operator report as awaiting a benchmark.

**Not yet measured by a retained, comparable workload:**

- The doorbell lane: paste, readback, Enter, and receipt against a real
  vendor CLI.
- The cold `cyclops` process start and the CLI send path. Those live in
  `release_transport_benchmark.rs` and need a frozen release candidate pair.
- Operating-system scheduler wakeups, idle CPU time, battery use, and memory
  growth over a long session. The quiet-pane workload counts only Cyclops
  application-level observation work during one bounded window.
- Terminal delivery under concurrency. The retained concurrent workload covers
  durable mailbox acceptance, not route selection, notification, or injection.
- Comparable Linux and macOS timing. The scheduled runner retains performance
  artifacts on Linux; the macOS matrix is correctness evidence.

**Known slow by design, and correctly so:** a doorbell held in gating waits
as long as a human draft is visible in the composer, a modal needs a person,
or a quota screen is up. Those are unbounded on purpose. The only clock on
them is `gate_hold_notify_ms`, and all it does is tell the admin the hold
exists.

## Reproducing every number

Machine context:

```bash
sysctl -n machdep.cpu.brand_string; sysctl -n hw.ncpu; sysctl -n hw.memsize
sw_vers -productVersion; tmux -V; rustc --version
```

The daemon evidence lanes. Every test in that binary is `#[ignore]`d and
`scripts/check.sh` does not build it; `scripts/ci-performance.py` is how the
scheduled lane invokes them and keeps their JSON.

```bash
cargo test --release -p cyclopsd --test evidence -- --ignored --nocapture \
  --test-threads=1 communication_benchmark::
cargo test --release -p cyclopsd --test evidence -- --ignored --nocapture \
  --test-threads=1 concurrent_messaging_perf::
cargo test --release -p cyclopsd --test evidence -- --ignored --nocapture \
  --test-threads=1 cold_start_replay_perf::
cargo test --release -p cyclopsd --test evidence -- --ignored --nocapture \
  --test-threads=1 idle_observation_perf::
```

Stream events, pane throughput, sustained output, input latency, render,
reconciliation, hydration, resize, and decoration coalescing:

```bash
cargo test --release -p cyclops-ui --test perf -- --nocapture --test-threads=1
cargo test --release -p cyclops-workspace --test baseline -- --nocapture --test-threads=1
cargo test --release -p cyclops-workspace --test perf_contract -- --nocapture --test-threads=1
cargo test --release -p cyclops-workspace --lib \
  render::canvas::tests::full_frame_paint_duration_scales_with_pane_count -- --nocapture
cargo test --release -p cyclops-workspace --lib \
  messages_pane_frame_time_with_100_messages -- --ignored --nocapture
```

`quitting_leaves_the_alternate_screen_and_returns_to_a_shell_prompt` in
`perf_contract.rs` drives the real binaries and fails until they are built.
Run `cargo build --release -p cyclops -p cyclopsd --bins` first, or read past
it: it produces no number on this page.

Every rig uses its own tmux server and its own home directory, and tears both
down afterwards. Never point one at a live session.
