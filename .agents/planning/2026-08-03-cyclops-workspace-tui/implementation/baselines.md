# Workspace performance baselines (A1a)

Numbers measured on this machine before R1 (runtime collapse), D2 (adapter
snapshots and concurrent hydration), and L1 (latency integration) touch any
code. The harness is `crates/cyclops-workspace/tests/baseline.rs`; run it
yourself with:

```
CARGO_INCREMENTAL=0 cargo test -p cyclops-workspace --test baseline -- --nocapture
```

These numbers are a record, not a gate: the test file asserts structure
(pane counts, command counts) but never a wall-clock budget. Rerun the
harness after R1/D2/L1 land and compare against the tables below.

## Machine context

Probed directly, copied from the terminal:

```
$ sysctl -n machdep.cpu.brand_string
Apple M1
$ sw_vers -productVersion
26.1
$ tmux -V
tmux 3.7b
$ rustc --version
rustc 1.97.1 (8bab26f4f 2026-07-14) (Homebrew)
$ sysctl -n hw.ncpu
8
```

## Measured numbers

Primary numbers are from a `--test-threads=1` run (the four tests in this
file otherwise run concurrently with each other and contend for the same
CPU, which the variance note below quantifies).

```
$ CARGO_INCREMENTAL=0 cargo test -p cyclops-workspace --test baseline -- --nocapture --test-threads=1
=== baseline: serial hydrate_pane latency (today's hydrate_visible_tab shape) ===
1 panes: total=0.36ms avg_per_pane=0.356ms per_pane=["0.36ms"]
4 panes: total=1.15ms avg_per_pane=0.286ms per_pane=["0.45ms", "0.26ms", "0.22ms", "0.22ms"]
8 panes: total=1.65ms avg_per_pane=0.206ms per_pane=["0.23ms", "0.23ms", "0.20ms", "0.21ms", "0.22ms", "0.20ms", "0.19ms", "0.19ms"]
=== baseline: PaneRuntime feed + grid-build throughput ===
80x24: fed 1048577 bytes over 257 frames in 75.93ms feed time (13.8 MB/s), avg grid build 165.79us/frame, total grid-build time 42.61ms
200x50: fed 1048577 bytes over 257 frames in 96.91ms feed time (10.8 MB/s), avg grid build 884.46us/frame, total grid-build time 227.31ms
=== baseline: reconciliation fan-out (list-sessions + membership + list-windows + list-panes/window) ===
1 windows: total=16.79ms commands_issued=4 (W+3 formula)
4 windows: total=27.72ms commands_issued=7 (W+3 formula)
8 windows: total=44.54ms commands_issued=11 (W+3 formula)
=== baseline: resize burst cost on a runtime holding scrollback ===
50 alternating resizes (80x24<->120x40) on a runtime with 2000 lines of scrollback: total=222.90ms avg=4457.44us max=6718us
```

### Hydration latency — baseline for D2 / L1

| Panes hydrated serially | Total | Avg/pane |
|---|---|---|
| 1 | 0.36ms | 0.36ms |
| 4 | 1.15ms | 0.29ms |
| 8 | 1.65ms | 0.21ms |

Today's `hydrate_visible_tab` (`crates/cyclops-workspace/src/sync.rs`) does
exactly this: one `hydrate_pane` after another. Total time here already
tracks roughly linearly with pane count (1.65ms at 8 panes vs. 0.36ms at 1).
D2/L1's concurrent hydration should make total time track the *slowest*
single pane instead — on this idle isolated rig that means 8-pane total
should approach the ~0.2ms single-pane figure rather than 8x it. On a real
session where panes hold agent TUIs (bigger captures, more history) the gap
between serial-sum and slowest-pane should be much larger than these
microbenchmark numbers suggest; this harness measures the *shape* of the
cost, not a promise about production magnitude.

### Reconciliation fan-out — baseline for D2

| Windows (W) | Total wall time | One-shot tmux processes issued |
|---|---|---|
| 1 | 16.79ms | 4 (W+3) |
| 4 | 27.72ms | 7 (W+3) |
| 8 | 44.54ms | 11 (W+3) |

`baseline_reconciliation_fan_out` calls the same public `cyclops-tmux`
functions `fetch_workspace_model`/`fetch_session_model` call today, in the
same order: one `list_sessions`, one `list_window_memberships`, one
`list_windows`, then one `list_panes` per window. The test asserts
`commands_issued == W + 3` structurally (not just prints it) — this is the
exact fan-out the recommendation names in "Replace the multi-process
workspace snapshot." Each of those calls is a freshly spawned `tmux`
one-shot process (`crates/cyclops-tmux/src/cmd.rs::run`), which is why wall
time climbs by roughly 10-15ms per extra window even though nothing here
touches the network or disk. D2's adapter-owned snapshot should collapse
this to a small, W-independent number of commands.

### PaneRuntime feed + grid-build throughput — floor for R1

| Runtime size | Bytes fed | Feed throughput | Avg grid build (`snapshot()`) |
|---|---|---|---|
| 80x24 | 1,048,577 (1 MiB) | 13.8 MB/s | 165.79us/frame |
| 200x50 | 1,048,577 (1 MiB) | 10.8 MB/s | 884.46us/frame |

Fed in 4096-byte chunks (257 chunks per run) mixing plain ASCII, SGR color
and attribute escapes, and CJK wide characters — one `snapshot()` call
(full grid build) per chunk, standing in for one render per control-mode
output batch. R1 proposes deleting the full-grid `CellGrid` mirror in favor
of visiting Alacritty's cells directly at the render boundary; these two
numbers (bytes/sec fed, and per-frame grid-build cost) are what that change
must not make worse. The grid-build cost scales with visible cell count as
expected (200x50 = 10,000 cells is ~5.3x 80x24's 1,920 cells and costs
~5.3x as much per frame: 884.46us vs. 165.79us).

### Resize cost with scrollback — cost for L1's coalescing to avoid paying repeatedly

| Alternating resizes (80x24 <-> 120x40) | Total | Avg | Max |
|---|---|---|---|
| 50, on a runtime holding 2000 lines of scrollback | 222.90ms | 4.46ms | 6.72ms |

This is the single most expensive operation measured in this harness: a
lone `resize()` call on a runtime with a couple thousand lines of history
costs multiple milliseconds — roughly 20-30x a single-pane hydration and
5-25x one frame's grid build. That is the cost the recommendation's
"Resize drag" contract ("at most one coalesced tmux resize per render
deadline") exists to stop paying on every intermediate drag position; a
resize storm that calls this once per mouse-move event rather than once per
coalesced deadline would be visibly slow at this cost per call.

## Variance under load (why these tests only record, never gate)

The same harness run with its four tests executing concurrently (the
default; no `--test-threads=1`) produced noticeably worse numbers on the
same idle-ish machine, purely from the tests contending with each other for
CPU:

```
4 panes: total=10.01ms avg_per_pane=2.499ms per_pane=["0.43ms", "0.49ms", "1.96ms", "7.12ms"]
50 alternating resizes (80x24<->120x40) on a runtime with 2000 lines of scrollback: total=668.12ms avg=13361.54us max=59036us
```

A single resize call spiked to 59ms under contention versus a 6.72ms max
in the isolated run — a ~9x swing from scheduling noise alone.
`crates/cyclops-ui/tests/perf.rs` already asserts a tight wall-clock budget
(`us < 16_000`) and is known to flake under exactly this kind of load; this
harness deliberately never repeats that mistake. Every number above is
`println!`-ed for a human or a later task to read and compare, and every
assertion in `baseline.rs` is structural (pane counts, window counts,
command counts) rather than a timing threshold.

## Post-R1 measurements (2026-08-05, same machine)

R1 deleted the full-grid `CellGrid` mirror (`cached_grid`/`grid_dirty`/
`CellGridView`) and made production rendering visit the engine's cells
directly (`PaneRuntime::for_each_visible_cell`), so the harness gained a
third metric: the direct cell walk, which *is* the post-R1 production
render path. Copied from a quiet run (`--test-threads=1`; an earlier run
during macOS storage-daemon churn was ~20% worse across the board,
including on unchanged code — trust only quiet-machine runs):

```
80x24: fed 1048577 bytes over 257 frames in 75.85ms feed time (13.8 MB/s), avg grid build 173.04us/frame, avg direct cell walk 150.75us/frame
200x50: fed 1048577 bytes over 257 frames in 92.63ms feed time (11.3 MB/s), avg grid build 888.31us/frame, avg direct cell walk 775.10us/frame
```

Against the pre-R1 baseline:

| Metric | Pre-R1 | Post-R1 | Reading |
|---|---|---|---|
| Feed, 80x24 | 13.8 MB/s | 13.8 MB/s | parity |
| Feed, 200x50 | 10.8 MB/s | 11.3 MB/s | parity/slightly better |
| Owned snapshot, 80x24 | 165.79us | 173.04us | parity (test-only path now) |
| Owned snapshot, 200x50 | 884.46us | 888.31us | parity |
| Production frame walk, 80x24 | 165.79us build + a second paint walk over the mirror | 150.75us, one pass | improved |
| Production frame walk, 200x50 | 884.46us build + a second paint walk | 775.10us, one pass | improved |

The pre-R1 production frame paid the mirror build *and then* a second
walk over the mirrored grid to paint (plus a third partial walk when a
selection was active). Post-R1 there is one walk, cheaper than the old
build alone, with the selection decision folded into the same visit.

## Post-L1 measurements (2026-08-05, same machine)

L1 replaced `crates/cyclops-workspace/src/sync.rs`'s `fetch_workspace_model`
fan-out with one `ControlClient::workspace_snapshot` call, made
`hydrate_visible_tab` hydrate every stale pane concurrently through
`ControlClient::hydrate_panes`, and coalesced the daemon decoration
subscription's per-event status fetch into one event-armed refresh per
burst (`app::spawn_decoration_forwarder`, `DECORATION_DEBOUNCE`). The harness
(`crates/cyclops-workspace/tests/baseline.rs`) gained two NEW tests that
measure the actual production path through the same fixtures the OLD-shape
tests already used, so both are recorded side by side rather than replacing
the before number:

```
$ CARGO_INCREMENTAL=0 cargo test -p cyclops-workspace --test baseline -- --nocapture --test-threads=1
test baseline_hydration_latency_concurrent ... === baseline: concurrent hydrate_panes latency (L1's hydrate_visible_tab shape) ===
1 panes: total=0.38ms (concurrent; tracks the slowest pane, not the sum)
4 panes: total=0.48ms (concurrent; tracks the slowest pane, not the sum)
8 panes: total=0.76ms (concurrent; tracks the slowest pane, not the sum)
ok
test baseline_hydration_latency_serial ... === baseline: serial hydrate_pane latency (today's hydrate_visible_tab shape) ===
1 panes: total=0.35ms avg_per_pane=0.349ms per_pane=["0.35ms"]
4 panes: total=1.24ms avg_per_pane=0.309ms per_pane=["0.36ms", "0.31ms", "0.29ms", "0.28ms"]
8 panes: total=2.44ms avg_per_pane=0.304ms per_pane=["0.38ms", "0.33ms", "0.31ms", "0.29ms", "0.29ms", "0.28ms", "0.28ms", "0.28ms"]
ok
test baseline_reconciliation_fan_out ... === baseline: reconciliation fan-out (list-sessions + membership + list-windows + list-panes/window) ===
1 windows: total=22.03ms commands_issued=4 (W+3 formula)
4 windows: total=38.23ms commands_issued=7 (W+3 formula)
8 windows: total=60.35ms commands_issued=11 (W+3 formula)
ok
test baseline_reconciliation_workspace_snapshot ... === baseline: workspace_snapshot (L1's fetch_workspace_model shape) ===
1 windows: total=0.32ms commands_issued=2 (fixed, not W+3)
4 windows: total=0.55ms commands_issued=2 (fixed, not W+3)
8 windows: total=0.87ms commands_issued=2 (fixed, not W+3)
ok
```

### Reconciliation: OLD fan-out vs. NEW workspace_snapshot

| Windows (W) | OLD (list-sessions + membership + list-windows + list-panes x W) | NEW (`workspace_snapshot`) | Commands: OLD | Commands: NEW |
|---|---|---|---|---|
| 1 | 22.03ms | 0.32ms | 4 (W+3) | 2 (fixed) |
| 4 | 38.23ms | 0.55ms | 7 (W+3) | 2 (fixed) |
| 8 | 60.35ms | 0.87ms | 11 (W+3) | 2 (fixed) |

At 8 windows this is a ~69x wall-clock improvement (60.35ms -> 0.87ms) and
the command count stops scaling with window count entirely — flat at 2
regardless of W, versus the old formula's `W + 3`. This is the same shape
D2 already proved at the adapter layer
(`crates/cyclops-tmux/tests/workspace_snapshot.rs`); the numbers above prove
the workspace crate's `fetch_workspace_model` is actually wired to it end to
end (see also the new rig test
`sync::tests::fetch_workspace_model_issues_a_bounded_command_count`, which
asserts the `commands_issued` delta by structure, not just prints it).

### Hydration: OLD serial vs. NEW concurrent

| Panes | OLD (serial `hydrate_pane` loop) | NEW (`hydrate_panes`, concurrent) |
|---|---|---|
| 1 | 0.35ms | 0.38ms |
| 4 | 1.24ms | 0.48ms |
| 8 | 2.44ms | 0.76ms |

At 8 panes the old serial loop (2.44ms) is roughly the sum of eight
~0.3ms round trips; the new concurrent path (0.76ms) tracks much closer to
one pane's round trip than to the sum of eight, exactly the shape the
recommendation asks for ("total time should track the slowest pane"). At 1
pane the two are within noise of each other (0.35ms vs. 0.38ms) — there is
nothing to overlap with a single pane, so this is the expected floor, not a
regression. On this idle isolated rig the absolute gap is small in wall
time; a real session hydrating panes that hold agent TUIs (bigger captures,
more scrollback) should widen the gap, the same caveat A1a's baseline
recorded for D2/L1's hydration numbers before any code changed.

### Decoration refresh coalescing

Not wall-clock-comparable to a "before" number the way the two tables above
are, because the old per-event-fetch code had no equivalent
"coalesce a burst" behavior to measure — every event cost its own fetch, so
there was no batching shape to time against. Proven instead by a new
deterministic rig test,
`app::tests::a_burst_of_decoration_events_produces_one_refresh`: a fake
daemon pushes five `events.subscribe` lines back to back with no delay
(the shape a split or a border drag produces), and the test asserts the
forwarder issued exactly one status fetch and delivered exactly one
`AppMsg::DecorationChanged` — not five. Passed in 5/5 repeated runs.

### Contention caveat

This machine was doing background work during every run above (Time
Machine's `backupd-helper` and Spotlight's `mds_stores`/`mdworker` were both
active per `ps aux`, matching the exact contention this file's "Variance
under load" section already documented for a different metric). Three
back-to-back runs of the full harness produced near-identical numbers for
every test above (hydration and reconciliation figures varied by <15%
across runs), but `baseline_pane_runtime_feed_and_grid_throughput` and
`baseline_resize_cost_with_scrollback` — unrelated to L1, unchanged since
the Post-R1 measurements above — read ~1.6-1.8x worse here (e.g. 80x24 grid
build 276.72us vs. the Post-R1 165.79-173.04us) than they did on this same
machine for A1a/R1. That gap is consistent with this file's existing
"macOS storage-daemon churn" note, not a regression from any L1 change:
L1 touched no code either of those two tests exercises. Trust the
before/after *shapes* (flat vs. growing command count; slowest-pane vs.
summed latency) over the absolute millisecond values, exactly as this
file's introduction already asks.

## Review finding 5 measurements (2026-08-06, same machine)

`src/cyclops-workspace/tests/perf_contract.rs` records these production-path
measurements without timing assertions. Run them with `--nocapture`; they are
a record, not a performance gate.

| Scenario | Result |
|---|---|
| Key-to-control write, idle pane (`send_keys_unconfirmed`, n=500) | p50 3.4us; p95 6–10us; max <170us |
| Key-to-control write, pane flooding ~8MB | p50 68us; p95 125us; max <800us |
| Sustained-output backlog | 93 drains at the 8ms cadence; peak batch 143 messages / 84KB; 7.0MB total; no stall longer than 5 empty cycles |
| Decoration burst | 100 signals in 0.6ms produced exactly one refresh 35.6ms after the first signal |
| Continuous decoration stream | 200ms of signals produced 6 refreshes at roughly 30–37ms gaps; the deadline arms once and is not pushed back |
| Full-frame paint, mixed ASCII/wide/SGR | median 0.99ms / 3.89ms / 7.78ms at 1 / 4 / 8 panes |
| Flow control, five runs | pause-to-confirmed-continue / continue-to-rehydrate: 0.30 / 1.33ms, 0.26 / 1.30ms, 0.20 / 2.57ms, 0.21 / 2.83ms, and 0.16 / 3.23ms |

The flooding path is roughly 20x slower at the median than the idle path,
but remains sub-millisecond. The 8-pane paint result stays inside the 8ms
debounce window.

These numbers came from a noisy machine and do not establish a user-visible
latency bound. In particular, the 4- and 8-pane paint figures are from one
quiet run; an earlier loaded run was discarded as noise. The harness measures
control writes, backlog drains, coalescing, and paint work, not loop-level
frame gaps under live output. The flow-control test now runs normally: tmux
3.7b's successful `refresh-client -A <pane>:continue` reply is the resume
confirmation when it omits `%continue`; the five recorded runs above use that
confirmed notification before rehydrating.

## Which task each baseline exists to check

| Baseline | Checked by | What "improved" means |
|---|---|---|
| Serial hydration latency (1/4/8 panes) | D2, L1 | Total hydration time for N visible panes tracks the slowest pane, not the sum of N serial round trips. |
| Reconciliation fan-out (list-sessions + membership + list-windows + list-panes x W) | D2, L1 | A full reconcile issues a small, bounded number of commands instead of W+3. |
| `PaneRuntime` feed/grid-build throughput | R1 | Deleting the full-grid `CellGrid` mirror does not reduce feed throughput or increase per-frame grid-build cost. |
| Resize cost with scrollback | C2 | Resize coalescing means a drag pays this cost once per render deadline, not once per intermediate mouse-move geometry. Corrected attribution: `app::apply_live_divider` (gated on the render debounce, one coalesced resize per deadline) landed in C2's executor-integration commit, before L1 started — L1's three integrations (batched reconciliation, concurrent hydration, decoration coalescing) did not touch resize handling. |

## Probe commands used

```
sysctl -n machdep.cpu.brand_string
sw_vers -productVersion
tmux -V
rustc --version
sysctl -n hw.ncpu
CARGO_INCREMENTAL=0 cargo test -p cyclops-workspace --test baseline -- --nocapture --test-threads=1
CARGO_INCREMENTAL=0 cargo test -p cyclops-workspace --test baseline -- --nocapture
```
