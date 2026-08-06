//! Performance baselines for the recommendation-plan reconciliation and
//! runtime refactors (task A1a).
//!
//! These tests RECORD numbers with `println!`; they never gate on a
//! wall-clock budget. `src/cyclops-ui/tests/perf.rs` asserts `us <
//! 16_000` on a frame build and that assertion has flaked under parallel
//! CPU load — a tight timing assertion in shared CI is a bug waiting for a
//! busy machine, not a useful gate. Run with `--nocapture` to see the
//! numbers:
//!
//! ```text
//! cargo test -p cyclops-workspace --test baseline -- --nocapture
//! ```
//!
//! Each measurement below exists to let a later task prove improvement
//! instead of claiming it:
//!
//! - Serial `hydrate_pane` latency is what D2 (concurrent hydration
//!   primitives) and L1 (integrating them) must beat.
//! - The multi-process reconciliation fan-out is what D2's adapter-owned
//!   snapshot must beat.
//! - `PaneRuntime` feed/grid throughput is what R1 (deleting the full-grid
//!   mirror in favor of direct Alacritty-cell rendering) must not regress.
//! - Resize cost on a runtime holding scrollback is the cost L1's resize
//!   coalescing is meant to stop paying repeatedly.
//!
//! L1 wired `cyclops-workspace/src/sync.rs` to the new adapter primitives, so
//! two more tests measure the NEW shape on the identical fixtures used
//! above, clearly labeled as such: `baseline_hydration_latency_concurrent`
//! (through `ControlClient::hydrate_panes`, what `hydrate_visible_tab` calls
//! today) and `baseline_reconciliation_workspace_snapshot` (through
//! `ControlClient::workspace_snapshot`, what `fetch_workspace_model` calls
//! today). The OLD-shape tests stay exactly as they were — they no longer
//! describe production code, but they are the only before/after comparison
//! point, so they are kept and relabeled rather than deleted.
//!
//! The tmux-backed tests use `cyclops_testrig::TmuxServer` and skip cleanly
//! when no tmux binary is on PATH, the same shape as
//! `src/cyclops-workspace/tests/hydration.rs`. The two pure tests touch
//! no tmux at all.

use std::time::{Duration, Instant};

use cyclops_testrig::{tmux_available, TmuxServer};
use cyclops_tmux::{
    list_panes, list_sessions, list_window_memberships, list_windows, ControlClient, ControlConfig,
};
use cyclops_workspace::PaneRuntime;

struct Rig {
    server: TmuxServer,
}

impl Rig {
    fn new(tag: &str) -> Option<Self> {
        if !tmux_available() {
            eprintln!("skipping: no tmux binary on PATH");
            return None;
        }
        Some(Self {
            server: TmuxServer::new(tag),
        })
    }

    fn config(&self, session: &str) -> ControlConfig {
        ControlConfig::attach(session)
            .on_socket(self.server.socket())
            .with_config_file("/dev/null")
    }

    /// One session, one window, tiled into `panes` panes by alternating
    /// horizontal/vertical splits of whichever pane is currently active.
    /// The tiling shape does not matter for these measurements, only that
    /// `panes` distinct pane ids exist.
    fn session_with_panes(&self, name: &str, panes: usize) {
        self.server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "220",
            "-y",
            "50",
            "/bin/sh",
        ]);
        for i in 1..panes {
            let dir = if i % 2 == 0 { "-h" } else { "-v" };
            self.server
                .run_ok(&["split-window", dir, "-t", name, "/bin/sh"]);
        }
    }

    /// One session with `windows` windows, one pane each — the shape a
    /// reconcile's `list-windows` + per-window `list-panes` walks today.
    fn session_with_windows(&self, name: &str, windows: usize) {
        self.server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "80",
            "-y",
            "24",
            "/bin/sh",
        ]);
        for _ in 1..windows {
            self.server.run_ok(&["new-window", "-t", name, "/bin/sh"]);
        }
    }

    fn pane_ids(&self, session: &str) -> Vec<String> {
        let out = self
            .server
            .run(&["list-panes", "-t", session, "-F", "#{pane_id}"]);
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// OLD shape, kept for comparison: `ControlClient::hydrate_pane` called
/// serially for every visible pane, the shape `hydrate_visible_tab` looped
/// through before L1
/// (`src/cyclops-workspace/src/sync.rs`). L1 replaced this with
/// `baseline_hydration_latency_concurrent` below, through
/// `ControlClient::hydrate_panes`; total latency should now track the
/// slowest pane instead of the sum of all of them.
#[tokio::test]
async fn baseline_hydration_latency_serial() {
    let Some(rig) = Rig::new("base-hyd") else {
        return;
    };
    println!("=== baseline: serial hydrate_pane latency (today's hydrate_visible_tab shape) ===");
    for &n in &[1usize, 4, 8] {
        let session = format!("hyd{n}");
        rig.session_with_panes(&session, n);
        let pane_ids = rig.pane_ids(&session);
        assert_eq!(
            pane_ids.len(),
            n,
            "expected {n} panes, tmux made {}",
            pane_ids.len()
        );

        // 1. Settle every pane's shell before the timed section starts, so
        //    shell-startup jitter never lands inside a measured hydrate.
        for (i, pane_id) in pane_ids.iter().enumerate() {
            rig.server.run_ok(&[
                "send-keys",
                "-t",
                pane_id,
                &format!("printf 'PANE_{i}_READY\\n'"),
                "Enter",
            ]);
        }
        rig.server.wait_screen(
            pane_ids.last().expect("at least one pane"),
            &format!("PANE_{}_READY", n - 1),
        );

        let (client, _notif) = ControlClient::spawn(rig.config(&session))
            .await
            .expect("attach");

        // 2. Hydrate every pane one at a time, timing each call and the
        //    total — exactly what hydrate_visible_tab does today.
        let total_start = Instant::now();
        let mut per_pane = Vec::with_capacity(n);
        for pane_id in &pane_ids {
            let t = Instant::now();
            client.hydrate_pane(pane_id).await.expect("hydrate_pane");
            per_pane.push(t.elapsed());
        }
        let total = total_start.elapsed();

        let sum_us: u128 = per_pane.iter().map(Duration::as_micros).sum();
        let avg_us = sum_us as f64 / n as f64;
        let per_pane_ms: Vec<String> = per_pane
            .iter()
            .map(|d| format!("{:.2}ms", d.as_secs_f64() * 1000.0))
            .collect();
        println!(
            "{n} panes: total={:.2}ms avg_per_pane={:.3}ms per_pane={per_pane_ms:?}",
            total.as_secs_f64() * 1000.0,
            avg_us / 1000.0
        );

        client.shutdown().await;
    }
}

/// NEW shape (L1): every stale pane's hydrate runs concurrently through one
/// `ControlClient::hydrate_panes` call instead of the serial loop above —
/// exactly what `src/cyclops-workspace/src/sync.rs`'s
/// `hydrate_visible_tab` calls today. Same fixtures and settle step as
/// `baseline_hydration_latency_serial`, so the two totals are directly
/// comparable.
#[tokio::test]
async fn baseline_hydration_latency_concurrent() {
    let Some(rig) = Rig::new("base-hyd-conc") else {
        return;
    };
    println!("=== baseline: concurrent hydrate_panes latency (L1's hydrate_visible_tab shape) ===");
    for &n in &[1usize, 4, 8] {
        let session = format!("hydc{n}");
        rig.session_with_panes(&session, n);
        let pane_ids = rig.pane_ids(&session);
        assert_eq!(
            pane_ids.len(),
            n,
            "expected {n} panes, tmux made {}",
            pane_ids.len()
        );

        // 1. Settle every pane's shell before the timed section starts, so
        //    shell-startup jitter never lands inside a measured hydrate.
        for (i, pane_id) in pane_ids.iter().enumerate() {
            rig.server.run_ok(&[
                "send-keys",
                "-t",
                pane_id,
                &format!("printf 'PANE_{i}_READY\\n'"),
                "Enter",
            ]);
        }
        rig.server.wait_screen(
            pane_ids.last().expect("at least one pane"),
            &format!("PANE_{}_READY", n - 1),
        );

        let (client, _notif) = ControlClient::spawn(rig.config(&session))
            .await
            .expect("attach");

        // 2. Hydrate every pane concurrently through one call — exactly
        //    what hydrate_visible_tab does today.
        let refs: Vec<&str> = pane_ids.iter().map(String::as_str).collect();
        let total_start = Instant::now();
        let results = client.hydrate_panes(&refs).await;
        let total = total_start.elapsed();
        for (i, r) in results.iter().enumerate() {
            r.as_ref()
                .unwrap_or_else(|e| panic!("pane {i} failed to hydrate: {e}"));
        }

        println!(
            "{n} panes: total={:.2}ms (concurrent; tracks the slowest pane, not the sum)",
            total.as_secs_f64() * 1000.0
        );

        client.shutdown().await;
    }
}

/// OLD shape, kept for comparison: the exact multi-process fan-out
/// `fetch_workspace_model` and `fetch_session_model` performed before L1
/// (`src/cyclops-workspace/src/sync.rs`): one `list-sessions`, one
/// all-window membership query, one `list-windows`, plus one `list-panes`
/// per window. `sync` is a private module, so this calls the same public
/// `cyclops-tmux` functions sync.rs used to call, in the same order, to
/// measure the identical fan-out. L1 replaced this with
/// `baseline_reconciliation_workspace_snapshot` below, through
/// `ControlClient::workspace_snapshot`.
#[test]
fn baseline_reconciliation_fan_out() {
    let Some(rig) = Rig::new("base-recon") else {
        return;
    };
    println!(
        "=== baseline: reconciliation fan-out (list-sessions + membership + list-windows + list-panes/window) ==="
    );
    for &w in &[1usize, 4, 8] {
        let session = format!("rec{w}");
        rig.session_with_windows(&session, w);
        let socket = Some(rig.server.socket());

        let t = Instant::now();
        // 1. One list-sessions.
        let sessions = list_sessions(socket).expect("list_sessions");
        // 2. One all-window membership query.
        let _memberships = list_window_memberships(socket).expect("list_window_memberships");
        // 3. One list-windows for the active session.
        let windows = list_windows(&session, socket).expect("list_windows");
        assert_eq!(windows.len(), w, "expected {w} windows");
        // 4. One list-panes per window: today's per-window loop.
        let mut pane_calls = 0usize;
        for win in &windows {
            let panes = list_panes(&win.id, socket).expect("list_panes");
            assert_eq!(panes.len(), 1, "one pane per window in this fixture");
            pane_calls += 1;
        }
        let elapsed = t.elapsed();

        assert!(sessions.iter().any(|s| s.name == session));
        let commands = 3 + pane_calls;
        assert_eq!(
            commands,
            w + 3,
            "the fan-out must issue exactly W+3 one-shot tmux processes"
        );
        println!(
            "{w} windows: total={:.2}ms commands_issued={commands} (W+3 formula)",
            elapsed.as_secs_f64() * 1000.0
        );
    }
}

/// NEW shape (L1): one `ControlClient::workspace_snapshot` round trip — two
/// control-mode commands over a connection that already exists, regardless
/// of window count — replaces the `W + 3` one-shot-process fan-out above.
/// This is exactly what `src/cyclops-workspace/src/sync.rs`'s
/// `fetch_workspace_model` calls today. Same fixtures as
/// `baseline_reconciliation_fan_out`, so the two totals are directly
/// comparable.
#[tokio::test]
async fn baseline_reconciliation_workspace_snapshot() {
    let Some(rig) = Rig::new("base-recon-snap") else {
        return;
    };
    println!("=== baseline: workspace_snapshot (L1's fetch_workspace_model shape) ===");
    // workspace_snapshot reads the whole server in two fixed commands, so
    // one client attached anywhere on the server measures every fixture
    // below — the same way one reconcile measures the whole workspace today.
    rig.session_with_windows("snap1", 1);
    let (client, _notif) = ControlClient::spawn(rig.config("snap1"))
        .await
        .expect("attach");
    for &w in &[1usize, 4, 8] {
        let session = format!("snap{w}");
        if w != 1 {
            rig.session_with_windows(&session, w);
        }

        let before = client.commands_issued();
        let t = Instant::now();
        let snapshot = client
            .workspace_snapshot()
            .await
            .expect("workspace_snapshot");
        let elapsed = t.elapsed();
        let after = client.commands_issued();

        let found = snapshot
            .sessions
            .iter()
            .find(|s| s.name == session)
            .unwrap_or_else(|| panic!("snapshot missing session {session}"));
        assert_eq!(found.windows.len(), w, "expected {w} windows");
        assert_eq!(
            after - before,
            2,
            "workspace_snapshot must cost exactly two commands regardless of window count"
        );

        println!(
            "{w} windows: total={:.2}ms commands_issued={} (fixed, not W+3)",
            elapsed.as_secs_f64() * 1000.0,
            after - before
        );
    }
    client.shutdown().await;
}

/// A synthetic byte stream at least `min_len` bytes, mixing plain ASCII,
/// SGR color/attribute escapes, and wide (CJK) characters — the shape of
/// real agent-TUI output this throughput benchmark stands in for.
fn synthetic_stream(min_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(min_len + 256);
    let mut i = 0u32;
    while out.len() < min_len {
        let line = match i % 4 {
            0 => format!("plain line {i} of steady output text\r\n"),
            1 => format!("\x1b[1;32mbold green line {i}\x1b[0m\r\n"),
            2 => format!("\x1b[38;2;200;90;10m宽字符行 {i} 中文测试内容\x1b[0m\r\n"),
            _ => format!("\x1b[4munderline {i}\x1b[24m plain tail\r\n"),
        };
        out.extend_from_slice(line.as_bytes());
        i += 1;
    }
    out
}

/// Baseline: `PaneRuntime::feed` throughput and per-frame `snapshot()`
/// (grid build) cost, pure — no tmux involved. R1's deletion of the
/// full-grid mirror in favor of direct Alacritty-cell rendering must not
/// regress either number.
#[test]
fn baseline_pane_runtime_feed_and_grid_throughput() {
    println!("=== baseline: PaneRuntime feed + grid-build throughput ===");
    let bytes = synthetic_stream(1024 * 1024);
    // 4096-byte chunks stand in for one control-mode %output batch reaching
    // the runtime between frames.
    const CHUNK: usize = 4096;
    for &(cols, rows) in &[(80u16, 24u16), (200u16, 50u16)] {
        let mut runtime = PaneRuntime::new(cols, rows);
        let mut feed_total = Duration::ZERO;
        let mut grid_total = Duration::ZERO;
        let mut walk_total = Duration::ZERO;
        let mut frames = 0usize;
        for chunk in bytes.chunks(CHUNK) {
            let t = Instant::now();
            runtime.feed(chunk);
            feed_total += t.elapsed();

            let t = Instant::now();
            let grid = runtime.snapshot();
            grid_total += t.elapsed();

            // The production render path after R1: visit engine cells
            // directly, no owned grid. The consumer folds each cell so the
            // optimizer cannot delete the walk.
            let t = Instant::now();
            let mut acc = 0usize;
            runtime.for_each_visible_cell(|_, _, cell| acc = acc.wrapping_add(cell.ch as usize));
            walk_total += t.elapsed();
            assert!(acc > 0, "the walk visited cells");

            frames += 1;
            assert!(
                grid.cols == cols && grid.rows == rows,
                "grid must keep the runtime's size"
            );
        }
        let bytes_per_sec = bytes.len() as f64 / feed_total.as_secs_f64();
        let avg_grid_us = grid_total.as_micros() as f64 / frames as f64;
        let avg_walk_us = walk_total.as_micros() as f64 / frames as f64;
        println!(
            "{cols}x{rows}: fed {} bytes over {frames} frames in {:.2}ms feed time ({:.1} MB/s), \
             avg grid build {avg_grid_us:.2}us/frame, avg direct cell walk {avg_walk_us:.2}us/frame",
            bytes.len(),
            feed_total.as_secs_f64() * 1000.0,
            bytes_per_sec / 1_000_000.0,
        );
    }
}

/// Baseline: a burst of alternating resizes on a runtime holding
/// scrollback. This is the repeated cost L1's resize coalescing (at most
/// one tmux resize per render deadline) exists to stop paying during a
/// resize drag.
#[test]
fn baseline_resize_cost_with_scrollback() {
    println!("=== baseline: resize burst cost on a runtime holding scrollback ===");
    let mut runtime = PaneRuntime::new(80, 24);
    // Feed several screens' worth of lines beyond the 24 visible rows so
    // the resizes below are not resizing an empty history.
    for i in 0..2000u32 {
        runtime.feed(format!("scrollback line {i}\r\n").as_bytes());
    }

    let sizes = [(120u16, 40u16), (80u16, 24u16)];
    let calls = 50;
    let mut durations = Vec::with_capacity(calls);
    let total_start = Instant::now();
    for n in 0..calls {
        let (cols, rows) = sizes[n % 2];
        let t = Instant::now();
        runtime.resize(cols, rows);
        durations.push(t.elapsed());
    }
    let total = total_start.elapsed();

    let sum_us: u128 = durations.iter().map(Duration::as_micros).sum();
    let avg_us = sum_us as f64 / durations.len() as f64;
    let max_us = durations.iter().map(Duration::as_micros).max().unwrap_or(0);
    println!(
        "{calls} alternating resizes (80x24<->120x40) on a runtime with 2000 lines of scrollback: \
         total={:.2}ms avg={:.2}us max={max_us}us",
        total.as_secs_f64() * 1000.0,
        avg_us
    );
}
