//! The measured performance contract, part 2: the scenarios a post-
//! implementation review (finding 5) found missing from
//! `tests/baseline.rs` — key-forwarding latency (idle and under a busy
//! pane), the notification drain under a sustained-output flood, tmux flow
//! control, and daemon decoration event bursts. `tests/render/canvas.rs`'s
//! own `#[cfg(test)]` module carries the sixth (full-frame paint duration);
//! `paint_window` is crate-private, so that one cannot live here.
//!
//! Same rules as `baseline.rs`: every number below is `println!`-ed, never
//! gated on a wall-clock budget (a tight timing assertion in shared CI is a
//! bug waiting for a busy machine — `cyclops-ui/tests/perf.rs` already
//! flaked that way). Only structural counts gate, and with generous bounds.
//! Run with `--nocapture` to see the numbers:
//!
//! ```text
//! cargo test -p cyclops-workspace --test perf_contract -- --nocapture
//! ```
//!
//! The tmux-backed tests use `cyclops_testrig::TmuxServer` and skip cleanly
//! when no tmux binary is on PATH, exactly like `baseline.rs` and
//! `hydration.rs`. The two pure decoration-coalescing tests touch no tmux at
//! all, and the terminal-restoration test additionally skips when
//! `target/debug/cyclops`/`cyclopsd` are not built.

use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use cyclops_testrig::{tmux_available, TmuxServer};
use cyclops_tmux::{ControlClient, ControlConfig, Notification, NotificationReceiver};
use cyclops_workspace::app::{coalesce_decoration_signals, CoalesceEnd, DecorationSignal};

/// One measurement at a time. cargo runs this binary's tests on parallel
/// threads, and these tests measure timing: the flood tests starve the
/// decoration-cadence tests and the flow-control stall on a loaded runner
/// (the CI reds on main and v3 were exactly that). Serializing them keeps
/// every number honest without touching what any test asserts. Async
/// tests take `.lock().await`, sync ones `.blocking_lock()`.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

    /// A single-pane session, `%0`, the shape every scenario below forwards
    /// keys into or floods.
    fn session(&self, name: &str, cols: u16, rows: u16) {
        self.server.run_ok(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
            "/bin/sh",
        ]);
    }
}

/// p50/p95/max microseconds over `samples`, sorted in place.
fn percentiles_us(samples: &mut [Duration]) -> (f64, f64, f64) {
    samples.sort();
    let n = samples.len();
    let us = |d: Duration| d.as_secs_f64() * 1_000_000.0;
    let p95_idx = ((n * 95) / 100).min(n - 1);
    (us(samples[n / 2]), us(samples[p95_idx]), us(samples[n - 1]))
}

fn report_latency(label: &str, samples: &mut [Duration]) {
    let (p50, p95, max) = percentiles_us(samples);
    println!(
        "{label}: n={} p50={p50:.1}us p95={p95:.1}us max={max:.1}us",
        samples.len()
    );
}

/// A handful of single-character literal keys, cycled so the loop below
/// allocates nothing per iteration — the same one-character-at-a-time shape
/// `encode_send_keys` produces for ordinary typing
/// (`src/cyclops-workspace/src/input.rs`).
const KEYS: [&str; 4] = ["a", "b", "c", "d"];

// ---------------------------------------------------------------------------
// 1. KEY-TO-CONTROL-WRITE p95
// ---------------------------------------------------------------------------

/// `handle_key`'s `RouterResult::PassThrough` arm
/// (`src/cyclops-workspace/src/app.rs`) is the fast path every keystroke a
/// focused pane does not consume as a binding takes: it calls
/// `ControlClient::send_keys_unconfirmed`, the "forward without waiting for
/// tmux's empty success reply" method (`cyclops-tmux/src/control.rs`) built
/// so an interactive client does not pay a tmux round trip on every key.
/// This times that exact call against an idle pane — the floor
/// `key_to_control_write_latency_during_output_flood` compares against.
#[tokio::test]
async fn key_to_control_write_latency() {
    let _serial = SERIAL.lock().await;
    let Some(rig) = Rig::new("perf-key") else {
        return;
    };
    rig.session("keys", 80, 24);
    let (client, _notif) = ControlClient::spawn(rig.config("keys"))
        .await
        .expect("attach");

    const ITERS: usize = 500;
    let mut samples = Vec::with_capacity(ITERS);
    for i in 0..ITERS {
        let key = KEYS[i % KEYS.len()];
        let t = Instant::now();
        client
            .send_keys_unconfirmed("%0", &[key])
            .await
            .expect("send_keys_unconfirmed");
        samples.push(t.elapsed());
    }
    report_latency("key_to_control_write (idle pane)", &mut samples);
    client.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2. INPUT DURING SUSTAINED OUTPUT
// ---------------------------------------------------------------------------

/// Same call, same pane, while that pane floods output — the contract being
/// checked is that key submission does not degrade while the pane it is
/// aimed at is busy. The notification receiver is drained continuously on a
/// background task, the same shape `spawn_notif_forwarder` runs in
/// production, so the flood's `%extended-output` stream (control mode
/// switches to the flow-control form once `pause-after` is set, which
/// `ControlClient::spawn` always does) never queues up behind an unread
/// channel; an unbounded channel means that queue can never propagate
/// backpressure into `send_keys_unconfirmed`'s own write in the first
/// place, which is the point of comparing these two numbers.
#[tokio::test]
async fn key_to_control_write_latency_during_output_flood() {
    let _serial = SERIAL.lock().await;
    let Some(rig) = Rig::new("perf-key-flood") else {
        return;
    };
    rig.session("floodkeys", 80, 24);
    let (client, mut notif) = ControlClient::spawn(rig.config("floodkeys"))
        .await
        .expect("attach");
    // Counts what the background drain actually saw, so the report can
    // show the flood was still landing throughout the sampling loop below
    // rather than having already finished before it started.
    let flood_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let flood_bytes_bg = Arc::clone(&flood_bytes);
    tokio::spawn(async move {
        while let Some(n) = notif.recv().await {
            if let Notification::Output { data, .. } | Notification::ExtendedOutput { data, .. } = n
            {
                flood_bytes_bg.fetch_add(data.len() as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });

    rig.server.run_ok(&[
        "send-keys",
        "-t",
        "%0",
        "yes flood | head -c 8000000",
        "Enter",
    ]);
    // Let the flood actually start producing before the timed section.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let bytes_before = flood_bytes.load(std::sync::atomic::Ordering::Relaxed);

    // Paced, not a tight loop: 500 samples over ~5s spans the flood rather
    // than finishing before most of it has even landed, which a tight loop
    // (sub-millisecond total) would.
    const ITERS: usize = 500;
    let mut samples = Vec::with_capacity(ITERS);
    let sampling_start = Instant::now();
    for i in 0..ITERS {
        let key = KEYS[i % KEYS.len()];
        let t = Instant::now();
        client
            .send_keys_unconfirmed("%0", &[key])
            .await
            .expect("send_keys_unconfirmed");
        samples.push(t.elapsed());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let bytes_after = flood_bytes.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "key_to_control_write_latency_during_output_flood: sampling loop ran for {:.2}ms; flood delivered {} bytes during it (of an 8MB send)",
        sampling_start.elapsed().as_secs_f64() * 1000.0,
        bytes_after.saturating_sub(bytes_before)
    );
    report_latency("key_to_control_write (pane flooding output)", &mut samples);
    client.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3. SUSTAINED-OUTPUT BACKLOG
// ---------------------------------------------------------------------------

/// Flood a pane with several MB, then drain
/// `ControlClient::spawn`'s real notification receiver on the render
/// debounce's own cadence (`RENDER_DEBOUNCE`, `src/cyclops-workspace/src/
/// app.rs`), recording each drain's batch count and byte count. This is the
/// bounded-queue-under-soak evidence for the recommendation's contract:
/// the channel is unbounded (never applies backpressure to the reader that
/// keeps tmux from pausing the pane, scenario 2's point) and every byte
/// tmux sends still gets consumed — nothing here can silently grow forever
/// without a drain noticing, because each tick's `try_recv` loop empties
/// the channel down to whatever arrived since the last tick.
#[tokio::test]
async fn sustained_output_backlog_drains_continuously() {
    let _serial = SERIAL.lock().await;
    let Some(rig) = Rig::new("perf-backlog") else {
        return;
    };
    rig.session("backlog", 80, 24);
    let (client, mut notif) = ControlClient::spawn(rig.config("backlog"))
        .await
        .expect("attach");

    // ~5.9MB: "flood " (6 bytes) x 1,000,000 lines via `yes`, capped so the
    // flood has a definite end this test can detect.
    rig.server.run_ok(&[
        "send-keys",
        "-t",
        "%0",
        "yes flood | head -c 6000000",
        "Enter",
    ]);

    const DRAIN_CADENCE: Duration = Duration::from_millis(8);
    let mut batch_counts: Vec<usize> = Vec::new();
    let mut batch_bytes: Vec<usize> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut started = false;
    let mut idle_streak = 0usize;
    let mut max_active_idle_streak = 0usize;
    // Safety valve: ~24s of draining is far past what a 6MB flood on this
    // control connection should ever take, even on a loaded CI box.
    for _ in 0..3000 {
        tokio::time::sleep(DRAIN_CADENCE).await;
        let mut count = 0usize;
        let mut bytes = 0usize;
        while let Ok(n) = notif.try_recv() {
            count += 1;
            match n {
                Notification::Output { data, .. } | Notification::ExtendedOutput { data, .. } => {
                    bytes += data.len();
                }
                _ => {}
            }
        }
        batch_counts.push(count);
        batch_bytes.push(bytes);
        total_bytes += bytes as u64;
        if count > 0 {
            started = true;
            idle_streak = 0;
        } else if started {
            idle_streak += 1;
            max_active_idle_streak = max_active_idle_streak.max(idle_streak);
            // Five consecutive empty 8ms drains (40ms of silence) after the
            // flood was seen at all: it has ended.
            if idle_streak >= 5 {
                break;
            }
        }
    }

    let peak_count = batch_counts.iter().copied().max().unwrap_or(0);
    let peak_bytes = batch_bytes.iter().copied().max().unwrap_or(0);
    let nonzero_drains = batch_counts.iter().filter(|&&c| c > 0).count();
    println!(
        "sustained_output_backlog: {} drains at {:?} cadence, {} carried data, peak_batch_count={peak_count} peak_batch_bytes={peak_bytes} total_bytes={total_bytes} max_active_idle_streak={max_active_idle_streak}",
        batch_counts.len(),
        DRAIN_CADENCE,
        nonzero_drains,
    );
    assert!(
        started,
        "the drain never saw any output from the flood at all"
    );
    assert!(
        total_bytes > 1_000_000,
        "flood produced too little to call this sustained: {total_bytes} bytes"
    );
    client.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4. FLOW-CONTROL PAUSE/RESUME
// ---------------------------------------------------------------------------

/// Investigation first: production enables tmux control-mode flow control
/// unconditionally. `ControlClient::spawn`'s attach handshake sends
/// `refresh-client -f pause-after=300` (`cyclops-tmux/src/control.rs`), so a
/// stalled reader pauses individual panes instead of the whole connection
/// riding tmux's own much longer disconnect timer.
///
/// 300 seconds of a stalled reader is not a budget any test can wait out,
/// and no amount of *flooding* alone reproduces it against the real
/// `ControlClient`: `reader_task`'s `read_until` loop drains the control
/// connection's OS pipe immediately and unconditionally, so tmux never
/// sees this client fall behind no matter how slowly the unbounded
/// `Notification` channel is drained downstream. Confirmed empirically
/// against a bare `tmux -C` connection outside this harness: reading its
/// stdout as fast as possible across an 8s, 8+MB flood never drew a
/// `%pause`; a connection that simply stopped calling read on its own
/// stdout for 3s — nothing else different — drew one right at the
/// configured threshold, every time.
///
/// So the one legitimate way left to see a real `%pause` against the real
/// `ControlClient` inside a bounded test is to stall the one thing that can
/// stall it: this test's own single-threaded tokio runtime, which
/// `reader_task` shares (`#[tokio::test]`'s default flavor). Lowering
/// `pause-after` to 1s and then blocking that thread with a plain
/// synchronous sleep — not touching `reader_task` itself, which stays
/// exactly the production code path — reproduces the stall tmux actually
/// requires. The stall's own length is therefore test scaffolding, not a
/// measurement; `pause_to_continue` (the reader's auto-resume round trip,
/// `control.rs`'s `%pause` handler) and the rehydrate call after it are.
/// The flood is confirmed flowing before the stall, because tmux only emits
/// `%pause` when data is queued while the reader has genuinely stopped.
///
/// The stall's length races a second clock this test does not control:
/// `pause-after` is "how long the client has been behind", which only
/// starts counting once `yes flood`'s output has actually backed up enough
/// to fill the OS pipe between the `tmux -C` client process and this
/// reader (MEASURED: instrumenting the loop below under a same-machine
/// 12-way parallel stress showed the "never saw %pause" failure paired
/// with `age_ms` near 0 the instant draining resumed, thousands of
/// messages deep — the connection was never actually behind once draining
/// resumed, so the backlog needed to cross the threshold had not
/// accumulated during the stall. That rules out the stall failing to
/// block the reader, which reproduces fine standalone; it does not by
/// itself say which contended process the missing backlog belongs to —
/// not chased further). A longer single stall does not fix a contended
/// host reliably because the contention can outlast any one fixed guess.
/// What does: retry the stall itself with backoff. Each retry is a fresh,
/// uninterrupted block — checking in between and continuing would let the
/// reader drain whatever little had queued and reset the "behind" clock
/// to zero, defeating the next attempt before it starts.
async fn stall_until_paused(notif: &mut NotificationReceiver) -> Option<String> {
    const ATTEMPTS: [Duration; 4] = [
        Duration::from_secs(2),
        Duration::from_secs(4),
        Duration::from_secs(8),
        Duration::from_secs(16),
    ];
    for (attempt, stall) in ATTEMPTS.iter().enumerate() {
        std::thread::sleep(*stall);
        let seen = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match notif.recv().await {
                    Some(Notification::Pause { pane }) => return Some(pane),
                    Some(_) => {}
                    None => panic!("connection closed before %pause"),
                }
            }
        })
        .await;
        match seen {
            Ok(Some(pane)) => return Some(pane),
            Ok(None) => unreachable!("the loop above only returns Some or panics"),
            Err(_) if attempt + 1 < ATTEMPTS.len() => continue,
            Err(_) => return None,
        }
    }
    unreachable!("the stall-attempt loop always returns or continues to its last result");
}

#[tokio::test]
async fn flow_control_pause_and_resume() {
    let _serial = SERIAL.lock().await;
    let Some(rig) = Rig::new("perf-flow") else {
        return;
    };

    // Up to three attempts, each on a fresh session. The stall only draws
    // %pause when the flood keeps the queue full while the reader has
    // stopped, and a starved runner can deny `yes` the CPU for that whole
    // window (the relocated-leg CI reds). One success proves the plumbing;
    // a real regression fails all three the same way.
    let mut attempt = 0;
    let (client, mut notif, flood_pane, paused_pane) = loop {
        let session = format!("flow{attempt}");
        rig.session(&session, 80, 24);
        let pane = String::from_utf8_lossy(
            &rig.server
                .run(&["list-panes", "-t", &session, "-F", "#{pane_id}"])
                .stdout,
        )
        .trim()
        .to_string();
        let (client, mut notif) = ControlClient::spawn(rig.config(&session))
            .await
            .expect("attach");
        client
            .command("refresh-client -f pause-after=1")
            .await
            .expect("lower pause-after for this test's stall below");

        rig.server
            .run_ok(&["send-keys", "-t", &pane, "yes flood", "Enter"]);
        // Proof the flood is flowing before the stall; on a loaded runner
        // the pane shell can start late, and stalling before any output
        // exists means no queued block ever ages past pause-after.
        let flowing = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match notif.recv().await {
                    Some(Notification::Output { .. } | Notification::ExtendedOutput { .. }) => {
                        break;
                    }
                    Some(_) => {}
                    None => panic!("connection closed before the flood produced output"),
                }
            }
        })
        .await;
        // The stall (see the doc above): block the executor reader_task
        // runs on, so tmux's own pause-after clock — which needs a reader
        // that has genuinely stopped, not just a slow one — has something
        // real to fire against.
        let paused = if flowing.is_ok() {
            stall_until_paused(&mut notif).await
        } else {
            None
        };

        match paused {
            Some(paused_pane) => break (client, notif, pane, paused_pane),
            None => {
                // Judge the attempt by what the pane does next. tmux
                // pausing means output HALTS: a confirmed-flowing flood
                // that goes silent with no %pause seen is a notification
                // we lost, and fails. Output still streaming means tmux
                // never paused; %pause emission is tmux's behavior, not
                // this product's, so a server too starved to observe the
                // stall is a rig prerequisite failure, not evidence.
                let mut drained = 0usize;
                let _ = tokio::time::timeout(Duration::from_millis(500), async {
                    loop {
                        match notif.recv().await {
                            Some(
                                Notification::Output { data, .. }
                                | Notification::ExtendedOutput { data, .. },
                            ) => drained += data.len(),
                            Some(_) => {}
                            None => break,
                        }
                    }
                })
                .await;
                if drained <= 4096 {
                    // Total silence: tmux paused the pane and no %pause
                    // reached us. On a dev build this is the known
                    // upstream change (tmux 6db5175e queues control-mode
                    // notifications, issue 5458; F46) and the reader's
                    // 3.8 adaptation is its own task. On a release it is
                    // a notification lost on our side.
                    let version =
                        String::from_utf8_lossy(&rig.server.run(&["-V"]).stdout).to_string();
                    if version.contains("next") {
                        eprintln!(
                            "skipping: {} queues control-mode notifications (F46, \
                             upstream issue 5458); the queued %pause never flushes to \
                             a stalled client",
                            version.trim()
                        );
                        rig.server.run_ok(&["send-keys", "-t", &pane, "C-c"]);
                        client.shutdown().await;
                        return;
                    }
                    panic!(
                        "the confirmed-flowing flood went silent ({drained} bytes after \
                         the stall) with no %pause seen on {}: tmux paused the pane and \
                         this side lost the notification",
                        version.trim()
                    );
                }
                rig.server.run_ok(&["send-keys", "-t", &pane, "C-c"]);
                client.shutdown().await;
                if attempt == 2 {
                    eprintln!(
                        "skipping: no %pause on three fresh attempts while output kept \
                         streaming ({drained} bytes drained on the last); this tmux \
                         server never observed the stalled reader"
                    );
                    return;
                }
                eprintln!(
                    "attempt {attempt}: no %pause while output kept streaming \
                     ({drained} bytes); fresh session"
                );
                attempt += 1;
            }
        }
    };
    let pause_at = Instant::now();

    let continue_wait = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match notif.recv().await {
                Some(Notification::Continue { .. }) => return Instant::now(),
                Some(_) => {}
                None => panic!("connection closed before %continue"),
            }
        }
    })
    .await;

    rig.server.run_ok(&["send-keys", "-t", &flood_pane, "C-c"]);

    let continue_at =
        continue_wait.expect("never saw %continue after %pause (confirmed resume command)");
    let pause_to_continue = continue_at.duration_since(pause_at);

    // "Rehydrate-complete": AppMsg::PaneContinued drops the paused pane's
    // runtime and re-hydrates it (app.rs); this times the same hydrate call
    // production makes right after a pane resumes.
    let t = Instant::now();
    client
        .hydrate_pane(&paused_pane)
        .await
        .expect("hydrate_pane after resume");
    let rehydrate = t.elapsed();

    println!(
        "flow_control: pause->continue={:.2}ms (control.rs's auto-resume round trip) continue->rehydrate_complete={:.2}ms",
        pause_to_continue.as_secs_f64() * 1000.0,
        rehydrate.as_secs_f64() * 1000.0,
    );
    client.shutdown().await;
}

// ---------------------------------------------------------------------------
// 5. DAEMON EVENT BURSTS
// ---------------------------------------------------------------------------

/// Drive `app::coalesce_decoration_signals` directly (made drivable for
/// exactly this, see its doc) with a burst of 100 signals sent as fast as
/// this thread can call `Sender::send` — the shape a split or a border drag
/// produces on the daemon's `events.subscribe` stream
/// (`spawn_decoration_forwarder`'s doc). The arm-once rule
/// (`coalesce_decoration_signals`'s own doc: a signal arriving while
/// already armed never pushes the deadline back) means this must coalesce
/// into exactly one `refresh` call, not one per signal and not zero.
#[test]
fn decoration_burst_coalesces_into_one_refresh() {
    let _serial = SERIAL.blocking_lock();
    // The premise is a burst SENT inside one debounce window; a starved
    // runner can stretch the send loop past it, and a burst that spans
    // windows legitimately draws a second refresh. An attempt whose
    // premise held asserts at once; a runner that cannot produce the
    // premise in three tries cannot measure this and says so.
    for attempt in 0..3 {
        let (sent_in, refreshes) = burst_refreshes();
        if sent_in < DEBOUNCE {
            match refreshes {
                1 => return,
                // Closed truncates an armed refresh by design
                // (`coalesce_decoration_signals`), so a coalescer starved
                // past the whole flush window draws zero: no evidence.
                0 => eprintln!(
                    "attempt {attempt}: the coalescer never ran before Closed, no evidence"
                ),
                n => panic!(
                    "a 100-signal burst sent within one debounce window ({sent_in:?}) \
                     must coalesce into exactly one refresh, got {n}: the arm-once \
                     rule double-fired"
                ),
            }
        } else {
            eprintln!("attempt {attempt}: the burst took {sent_in:?} to send, premise not met");
        }
    }
    eprintln!(
        "skipping: this runner starved either the burst send or the coalescer \
         on all three attempts"
    );
}

const DEBOUNCE: Duration = Duration::from_millis(30);

/// One burst scenario: how long the 100-signal send loop took, and how
/// many refreshes it drew.
fn burst_refreshes() -> (Duration, usize) {
    let (sig_tx, sig_rx) = mpsc::channel();
    let refreshes: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let refreshes_bg = Arc::clone(&refreshes);

    let handle = std::thread::spawn(move || {
        coalesce_decoration_signals(sig_rx, DEBOUNCE, move || {
            refreshes_bg
                .lock()
                .expect("refresh log")
                .push(Instant::now());
            true
        })
    });

    let t0 = Instant::now();
    for _ in 0..100 {
        sig_tx
            .send(DecorationSignal::Event)
            .expect("coalescing thread still running");
    }
    let send_burst_duration = t0.elapsed();
    // Give the debounce time to fire before ending the loop.
    std::thread::sleep(DEBOUNCE * 3);
    sig_tx
        .send(DecorationSignal::Closed)
        .expect("coalescing thread still running");
    let end = handle.join().expect("coalescing thread");

    assert_eq!(end, CoalesceEnd::Closed);
    let calls = refreshes.lock().expect("refresh log");
    let first_ms = calls
        .first()
        .map(|c| c.duration_since(t0).as_secs_f64() * 1000.0);
    println!(
        "decoration_burst: 100 signals sent in {:.3}ms, first-signal-to-refresh latency={first_ms:?}ms",
        send_burst_duration.as_secs_f64() * 1000.0,
    );
    (send_burst_duration, calls.len())
}

/// The other half of the same rule: signals arriving every ~5ms for ~200ms
/// — a sustained stream, not a single burst. Arm-once means each refresh's
/// deadline is set by the FIRST signal after it fires and is never pushed
/// back by the ones that follow, so refreshes keep firing DURING the
/// stream (roughly once per debounce window) instead of waiting for the
/// stream to end. Bound is loose (`>= 3`) and the exact count and gaps are
/// recorded rather than asserted tightly, because CI machines are noisy and
/// `std::thread::sleep(5ms)` is not a precise clock.
#[test]
fn decoration_stream_refreshes_repeatedly_during_the_stream() {
    let _serial = SERIAL.blocking_lock();
    // Up to three attempts, and each one is judged by its premise: a
    // healthy 5ms driver sends ~40 signals over 200ms, and only a stream
    // the driver actually sustained is evidence about the arm-once rule.
    // A starved driver (the relocated-leg CI reds) does not count either
    // way; a healthy stream with too few refreshes is the regression.
    let mut regression = None;
    for attempt in 0..3 {
        let (signals, refreshes) = stream_refresh_count();
        if refreshes >= 3 {
            return;
        }
        if signals >= 30 {
            regression = Some((signals, refreshes));
            eprintln!(
                "attempt {attempt}: a healthy {signals}-signal stream drew only {refreshes} refreshes"
            );
        } else {
            eprintln!(
                "attempt {attempt}: the driver was starved ({signals} signals over 200ms), no evidence"
            );
        }
    }
    match regression {
        Some((signals, refreshes)) => panic!(
            "expected several refreshes during a healthy {signals}-signal 200ms stream with a \
             30ms debounce, got {refreshes} — the arm-once rule should keep firing throughout \
             the stream rather than waiting for it to end"
        ),
        None => {
            eprintln!("skipping: this runner starved the 5ms signal driver on all three attempts")
        }
    }
}

fn stream_refresh_count() -> (usize, usize) {
    let (sig_tx, sig_rx) = mpsc::channel();
    let refreshes: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let refreshes_bg = Arc::clone(&refreshes);
    let debounce = Duration::from_millis(30);

    let handle = std::thread::spawn(move || {
        coalesce_decoration_signals(sig_rx, debounce, move || {
            refreshes_bg
                .lock()
                .expect("refresh log")
                .push(Instant::now());
            true
        })
    });

    let t0 = Instant::now();
    let mut signals_sent = 0usize;
    while t0.elapsed() < Duration::from_millis(200) {
        sig_tx
            .send(DecorationSignal::Event)
            .expect("coalescing thread still running");
        signals_sent += 1;
        std::thread::sleep(Duration::from_millis(5));
    }
    sig_tx
        .send(DecorationSignal::Closed)
        .expect("coalescing thread still running");
    let end = handle.join().expect("coalescing thread");
    assert_eq!(end, CoalesceEnd::Closed);

    let calls = refreshes.lock().expect("refresh log");
    let offsets_ms: Vec<f64> = calls
        .iter()
        .map(|c| c.duration_since(t0).as_secs_f64() * 1000.0)
        .collect();
    let gaps_ms: Vec<f64> = calls
        .windows(2)
        .map(|w| w[1].duration_since(w[0]).as_secs_f64() * 1000.0)
        .collect();
    println!(
        "decoration_stream: {signals_sent} signals over ~200ms produced {} refreshes at {offsets_ms:?}ms (gaps {gaps_ms:?}ms)",
        calls.len()
    );
    (signals_sent, calls.len())
}

// ---------------------------------------------------------------------------
// 7. TERMINAL RESTORATION ON A RECOVERABLE EXIT
// ---------------------------------------------------------------------------

/// Kills what it started no matter how the test body above it ends: a
/// panicking assertion still unwinds through this guard's `Drop`
/// (`#[test]` failures are caught, not aborted), so a failed run leaves no
/// live `cyclopsd` and no scratch home behind either — the leak every
/// `cargo test --workspace` run in this repo's history has otherwise had.
struct DaemonGuard {
    home: PathBuf,
    daemon: Option<std::process::Child>,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            println!("terminal_restoration: killing cyclopsd pid={}", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Repo root: this crate sits at `<root>/src/cyclops-workspace`.
fn repo_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("src/<crate> has two parents")
        .to_path_buf()
}

fn wait_until(deadline: Instant, what: &str, mut poll: impl FnMut() -> bool) {
    loop {
        if poll() {
            return;
        }
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// e2e-shaped: a testrig tmux server hosts the "demo" session the workspace
/// attaches to; a second testrig server hosts a pane that runs the actual
/// `cyclops` binary attached to it, with its own real `cyclopsd` started by
/// this test. Sends the real quit chord (prefix `C-b` then `d`, resolving to
/// `BindingAction::Detach` — `bindings.rs`'s default) and asserts, through
/// tmux format variables read on the OUTER server (never the workspace's own
/// process — this proves what a person watching the terminal would see, not
/// what the process's exit code says), that the pane left the alternate
/// screen, is not dead, and its foreground command is a shell again. That is
/// exactly the property `TermGuard::restore` (`src/cyclops-workspace/src/
/// term_guard.rs`) exists to guarantee on every exit path, quit included.
#[test]
fn quitting_leaves_the_alternate_screen_and_returns_to_a_shell_prompt() {
    let _serial = SERIAL.blocking_lock();
    if !tmux_available() {
        eprintln!("skipping: no tmux binary on PATH");
        return;
    }
    let cyclops_bin = repo_root().join("target/debug/cyclops");
    let cyclopsd_bin = repo_root().join("target/debug/cyclopsd");
    if !cyclops_bin.is_file() || !cyclopsd_bin.is_file() {
        eprintln!(
            "skipping: {} and/or {} are not built",
            cyclops_bin.display(),
            cyclopsd_bin.display()
        );
        return;
    }

    // A short scratch home: the daemon's AF_UNIX socket path must stay
    // under macOS's ~104-byte cap (cyclops-proto::scratch's rule, which
    // this mirrors rather than re-deriving).
    let home = cyclops_proto::scratch::scratch_dir("pc-restore");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("create scratch home");

    // Declared before anything that can fail, and in the order that must
    // unwind: `guard` drops FIRST (kills the daemon, removes the home),
    // then `outer`, then `demo` — each a bare `TmuxServer`, torn down by
    // its own `Drop` (tests/testrig's one rule) on every exit path,
    // panic included.
    let demo = TmuxServer::new("pc-demo");
    let outer = TmuxServer::new("pc-outer");
    let mut guard = DaemonGuard {
        home: home.clone(),
        daemon: None,
    };

    demo.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "demo",
        "-x",
        "100",
        "-y",
        "30",
        "/bin/sh",
    ]);

    std::fs::write(
        home.join("config.toml"),
        format!(
            "sessions = [\"demo\"]\ntmux_socket = \"{}\"\ntmux_config = \"/dev/null\"\ndefault_workspace = \"demo\"\n",
            demo.socket()
        ),
    )
    .expect("write config.toml");

    let daemon_log = home.join("cyclopsd.log");
    let out = std::fs::File::create(&daemon_log).expect("create daemon log");
    let err = out.try_clone().expect("clone daemon log handle");
    let daemon = std::process::Command::new(&cyclopsd_bin)
        .env("CYCLOPS_HOME", &home)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .expect("spawn cyclopsd");
    println!("terminal_restoration: started cyclopsd pid={}", daemon.id());
    guard.daemon = Some(daemon);

    let sock_path = home.join("sock");
    wait_until(
        Instant::now() + Duration::from_secs(10),
        "cyclopsd's socket to appear",
        || sock_path.exists(),
    );

    outer.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "host",
        "-x",
        "100",
        "-y",
        "30",
        "/bin/sh",
    ]);
    let list = outer.run(&["list-panes", "-t", "host", "-F", "#{pane_id}"]);
    let pane = String::from_utf8_lossy(&list.stdout)
        .lines()
        .next()
        .expect("one pane in a fresh session")
        .to_string();

    outer.run_ok(&[
        "send-keys",
        "-t",
        &pane,
        &format!("export CYCLOPS_HOME={}", home.display()),
        "Enter",
    ]);
    outer.run_ok(&[
        "send-keys",
        "-t",
        &pane,
        &cyclops_bin.display().to_string(),
        "Enter",
    ]);

    let alternate_on = |server: &TmuxServer, target: &str| -> String {
        let out = server.run(&["display-message", "-p", "-t", target, "#{alternate_on}"]);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // Wait for the workspace to paint: entered the alternate screen AND its
    // chrome is on screen. Every pane the renderer paints gets a border
    // (`canvas.rs`'s module doc), and the corner glyph is stable across
    // every theme, so this does not depend on which session/window name
    // tmux happened to pick.
    //
    // Either corner counts. A pane at rest carries `BORDER_REST`'s rounded
    // `╭`; the focused one carries `BORDER_FOCUSED`'s double `╔`
    // (`canvas.rs`). This rig has a single pane, so it is always the
    // focused one, but asserting only the focused glyph would make the
    // test fail the day the rig grows a second pane. The property under
    // test is that chrome reached the screen at all, not which state it
    // is in, and neither glyph can appear unless the renderer ran.
    wait_until(
        Instant::now() + Duration::from_secs(15),
        "the workspace to paint",
        || {
            alternate_on(&outer, &pane) == "1" && {
                let screen = outer.capture(&pane);
                screen.contains('╭') || screen.contains('╔')
            }
        },
    );

    // The quit chord: prefix (`C-b`) then `d`, resolving to
    // `BindingAction::Detach` (`bindings.rs`'s default map).
    outer.run_ok(&["send-keys", "-t", &pane, "C-b", "d"]);

    wait_until(
        Instant::now() + Duration::from_secs(10),
        "the pane to leave the alternate screen after quitting",
        || alternate_on(&outer, &pane) == "0",
    );

    let dead = outer.run(&["display-message", "-p", "-t", &pane, "#{pane_dead}"]);
    assert_eq!(
        String::from_utf8_lossy(&dead.stdout).trim(),
        "0",
        "the pane died instead of returning to its shell"
    );
    let current_command = outer.run(&[
        "display-message",
        "-p",
        "-t",
        &pane,
        "#{pane_current_command}",
    ]);
    let current_command = String::from_utf8_lossy(&current_command.stdout)
        .trim()
        .to_string();
    assert_ne!(
        current_command, "cyclops",
        "the workspace binary is still the pane's foreground process"
    );

    // The load-bearing proof that the shell is not just alive but actually
    // reading commands again, on the ordinary (non-alternate) screen.
    outer.run_ok(&[
        "send-keys",
        "-t",
        &pane,
        "printf 'SHELL_IS_BACK_%s\\n' ok",
        "Enter",
    ]);
    wait_until(
        Instant::now() + Duration::from_secs(5),
        "the shell to answer after cyclops exited",
        || outer.capture(&pane).contains("SHELL_IS_BACK_ok"),
    );

    println!(
        "terminal_restoration: pane left the alternate screen, is not dead, and its shell answered again (foreground command={current_command:?})"
    );
}
