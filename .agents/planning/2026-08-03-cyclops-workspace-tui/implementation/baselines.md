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

## Which task each baseline exists to check

| Baseline | Checked by | What "improved" means |
|---|---|---|
| Serial hydration latency (1/4/8 panes) | D2, L1 | Total hydration time for N visible panes tracks the slowest pane, not the sum of N serial round trips. |
| Reconciliation fan-out (list-sessions + membership + list-windows + list-panes x W) | D2 | A full reconcile issues a small, bounded number of commands instead of W+3. |
| `PaneRuntime` feed/grid-build throughput | R1 | Deleting the full-grid `CellGrid` mirror does not reduce feed throughput or increase per-frame grid-build cost. |
| Resize cost with scrollback | L1 | Resize coalescing means a drag pays this cost once per render deadline, not once per intermediate mouse-move geometry. |

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
