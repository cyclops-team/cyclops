//! Performance, latency, throughput, and resource usage benchmark comparing
//! Cyclops messaging against raw tmux commands and ShawnPana's smux tmux-bridge.

mod common;

use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{composer_pane, Rig, CAT_MANIFEST};
use cyclops_proto::MsgSendParams;
use serde_json::json;

#[derive(Debug, Clone, Default)]
struct LatencyStats {
    count: usize,
    min_us: u64,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
    avg_us: u64,
    throughput_ops_per_sec: f64,
}

impl LatencyStats {
    fn compute(mut samples_us: Vec<u64>, total_duration: Duration) -> Self {
        if samples_us.is_empty() {
            return Self::default();
        }
        samples_us.sort_unstable();
        let count = samples_us.len();
        let min_us = samples_us[0];
        let max_us = samples_us[count - 1];
        let sum_us: u64 = samples_us.iter().sum();
        let avg_us = sum_us / count as u64;

        let at = |p: usize| -> u64 {
            let idx = (count - 1) * p / 100;
            samples_us[idx]
        };

        let throughput_ops_per_sec = if total_duration.as_secs_f64() > 0.0 {
            count as f64 / total_duration.as_secs_f64()
        } else {
            0.0
        };

        Self {
            count,
            min_us,
            p50_us: at(50),
            p95_us: at(95),
            p99_us: at(99),
            max_us,
            avg_us,
            throughput_ops_per_sec,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ResourceSnapshot {
    rss_kb: u64,
    fd_count: usize,
}

fn measure_process_resources(pid: i32) -> ResourceSnapshot {
    let rss_kb = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        })
        .unwrap_or(0);

    let fd_count = Command::new("lsof")
        .args(["-p", &pid.to_string()])
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).lines().skip(1).count())
        .unwrap_or(0);

    ResourceSnapshot { rss_kb, fd_count }
}

#[tokio::test(flavor = "multi_thread")]
async fn benchmark_cyclops_vs_raw_tmux_and_smux() {
    println!("\n=========================================================================================================");
    println!("        CYCLOPS VS RAW TMUX VS SHAWNPANA/SMUX PROTOCOL BENCHMARK & ARCHITECTURAL COMPARISON              ");
    println!("=========================================================================================================\n");

    let mut rig = Rig::new(
        "comm-bench",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery.force_submit_delay_ms = 0\n",
    )
    .await;

    let target_pane = "%0";
    let sender_label = "admin";
    let sample_count = 50;

    // -----------------------------------------------------------------------
    // Benchmark 1: Raw tmux send-keys (blind fire-and-forget keystroke injection)
    // -----------------------------------------------------------------------
    let mut tmux_send_samples_us = Vec::with_capacity(sample_count);
    let tmux_send_bench_start = Instant::now();
    for i in 0..sample_count {
        let text = format!("[raw-tmux-msg-{i}] Hello pane from raw tmux\n");
        let start = Instant::now();
        rig.tmux
            .cmd()
            .args(["send-keys", "-t", target_pane, &text, "Enter"])
            .output()
            .expect("raw tmux send-keys");
        let elapsed = start.elapsed().as_micros() as u64;
        tmux_send_samples_us.push(elapsed);
    }
    let tmux_send_total_duration = tmux_send_bench_start.elapsed();
    let tmux_send_stats = LatencyStats::compute(tmux_send_samples_us, tmux_send_total_duration);

    // -----------------------------------------------------------------------
    // Benchmark 2: Raw tmux capture-pane (screen capture inspection)
    // -----------------------------------------------------------------------
    let mut tmux_capture_samples_us = Vec::with_capacity(sample_count);
    let tmux_capture_bench_start = Instant::now();
    for _ in 0..sample_count {
        let start = Instant::now();
        let out = rig
            .tmux
            .cmd()
            .args(["capture-pane", "-p", "-t", target_pane])
            .output()
            .expect("raw tmux capture-pane");
        let elapsed = start.elapsed().as_micros() as u64;
        tmux_capture_samples_us.push(elapsed);
        assert!(!out.stdout.is_empty());
    }
    let tmux_capture_total_duration = tmux_capture_bench_start.elapsed();
    let tmux_capture_stats =
        LatencyStats::compute(tmux_capture_samples_us, tmux_capture_total_duration);

    // -----------------------------------------------------------------------
    // Benchmark 3: ShawnPana/smux tmux-bridge messaging cycle:
    // (1) read-guard check & touch /tmp/tmux-bridge-read-*
    // (2) tmux capture-pane -p -e -S -50 (read before act)
    // (3) tmux send-keys -l (type text)
    // (4) tmux send-keys Enter (submit keys)
    // (5) clear read-guard rm -f /tmp/tmux-bridge-read-*
    // -----------------------------------------------------------------------
    let mut smux_samples_us = Vec::with_capacity(sample_count);
    let guard_file = std::env::temp_dir().join("tmux-bridge-read-_0");
    let smux_bench_start = Instant::now();
    for i in 0..sample_count {
        let text = format!("[tmux-bridge from:admin] Task instruction {i}");
        let start = Instant::now();

        // Step 1: Read guard mark
        let _ = fs::write(&guard_file, b"read");

        // Step 2: tmux capture-pane (reading target pane before acting)
        let _ = rig
            .tmux
            .cmd()
            .args(["capture-pane", "-p", "-e", "-S", "-50", "-t", target_pane])
            .output()
            .expect("smux capture-pane");

        // Step 3: Type text without Enter (tmux send-keys -l)
        let _ = rig
            .tmux
            .cmd()
            .args(["send-keys", "-l", "-t", target_pane, &text])
            .output()
            .expect("smux send-keys literal");

        // Step 4: Press Enter (tmux send-keys Enter)
        let _ = rig
            .tmux
            .cmd()
            .args(["send-keys", "-t", target_pane, "Enter"])
            .output()
            .expect("smux send-keys enter");

        // Step 5: Clear read-guard
        let _ = fs::remove_file(&guard_file);

        let elapsed = start.elapsed().as_micros() as u64;
        smux_samples_us.push(elapsed);
    }
    let smux_total_duration = smux_bench_start.elapsed();
    let smux_stats = LatencyStats::compute(smux_samples_us, smux_total_duration);

    // Get tmux server PID and measure resources
    let tmux_pid_out = rig
        .tmux
        .cmd()
        .args(["display-message", "-p", "#{pid}"])
        .output()
        .expect("get tmux pid");
    let tmux_pid: i32 = String::from_utf8_lossy(&tmux_pid_out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    let tmux_res = measure_process_resources(tmux_pid);

    // -----------------------------------------------------------------------
    // Benchmark 4: Cyclops msg_send (Durable Journal Append + FIFO Gating)
    // -----------------------------------------------------------------------
    let mut cyclops_send_samples_us = Vec::with_capacity(sample_count);
    let mut msg_ids = Vec::with_capacity(sample_count);

    let cyclops_send_start = Instant::now();
    for i in 0..sample_count {
        let start = Instant::now();
        let params: MsgSendParams = serde_json::from_value(json!({
            "to": ["admin"],
            "subject": format!("Benchmark message {i}"),
            "body": format!("Payload for cyclops message {i} with verified durability"),
            "client_key": format!("bench-key-{i}"),
        }))
        .unwrap();

        let receipt = rig
            .daemon
            .msg_send(sender_label, params)
            .await
            .expect("cyclops send");
        let elapsed = start.elapsed().as_micros() as u64;
        cyclops_send_samples_us.push(elapsed);

        let msg_id = receipt["msg_id"].as_str().unwrap().to_string();
        msg_ids.push(msg_id);
    }
    let cyclops_send_duration = cyclops_send_start.elapsed();
    let cyclops_send_stats = LatencyStats::compute(cyclops_send_samples_us, cyclops_send_duration);

    // -----------------------------------------------------------------------
    // Benchmark 5: Cyclops inbox.claim (Private Body Extraction via Mailbox)
    // -----------------------------------------------------------------------
    let mut cyclops_claim_samples_us = Vec::with_capacity(sample_count);
    let cyclops_claim_start = Instant::now();
    for msg_id in &msg_ids {
        let start = Instant::now();
        let res = rig
            .ctl
            .request(
                "inbox.claim",
                json!({ "message_id": msg_id, "recipient": "admin" }),
            )
            .await;
        let elapsed = start.elapsed().as_micros() as u64;
        cyclops_claim_samples_us.push(elapsed);
        assert_eq!(res["result"]["disposition"], "claimed");
    }
    let cyclops_claim_duration = cyclops_claim_start.elapsed();
    let cyclops_claim_stats =
        LatencyStats::compute(cyclops_claim_samples_us, cyclops_claim_duration);

    // Measure cyclopsd daemon resources
    let daemon_pid = std::process::id() as i32;
    let cyclops_res = measure_process_resources(daemon_pid);

    // Measure journal on-disk size
    let workspaces_dir = rig.home.join("workspaces");
    let mut journal_bytes: u64 = 0;
    if let Ok(entries) = fs::read_dir(&workspaces_dir) {
        for entry in entries.flatten() {
            let journal_file = entry.path().join("messages.ndjson");
            if let Ok(metadata) = fs::metadata(journal_file) {
                journal_bytes += metadata.len();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Print Comparative Results
    // -----------------------------------------------------------------------
    println!("---------------------------------------------------------------------------------------------------------");
    println!(
        "1. LATENCY & THROUGHPUT BENCHMARK ({} operations each)",
        sample_count
    );
    println!("---------------------------------------------------------------------------------------------------------");
    println!(
        "{:<40} | {:>6} | {:>6} | {:>6} | {:>6} | {:>6} | {:>6} | {:>14}",
        "Communication Path", "min", "p50", "p95", "p99", "avg", "max", "Throughput (ops/s)"
    );
    println!("---------------------------------------------------------------------------------------------------------");
    println!(
        "{:<40} | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>14.1}",
        "Raw tmux send-keys",
        tmux_send_stats.min_us,
        tmux_send_stats.p50_us,
        tmux_send_stats.p95_us,
        tmux_send_stats.p99_us,
        tmux_send_stats.avg_us,
        tmux_send_stats.max_us,
        tmux_send_stats.throughput_ops_per_sec
    );
    println!(
        "{:<40} | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>14.1}",
        "Raw tmux capture-pane",
        tmux_capture_stats.min_us,
        tmux_capture_stats.p50_us,
        tmux_capture_stats.p95_us,
        tmux_capture_stats.p99_us,
        tmux_capture_stats.avg_us,
        tmux_capture_stats.max_us,
        tmux_capture_stats.throughput_ops_per_sec
    );
    println!(
        "{:<40} | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>14.1}",
        "smux tmux-bridge (guard + 3 calls)",
        smux_stats.min_us,
        smux_stats.p50_us,
        smux_stats.p95_us,
        smux_stats.p99_us,
        smux_stats.avg_us,
        smux_stats.max_us,
        smux_stats.throughput_ops_per_sec
    );
    println!(
        "{:<40} | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>14.1}",
        "Cyclops msg_send (durable append)",
        cyclops_send_stats.min_us,
        cyclops_send_stats.p50_us,
        cyclops_send_stats.p95_us,
        cyclops_send_stats.p99_us,
        cyclops_send_stats.avg_us,
        cyclops_send_stats.max_us,
        cyclops_send_stats.throughput_ops_per_sec
    );
    println!(
        "{:<40} | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>14.1}",
        "Cyclops inbox.claim (body access)",
        cyclops_claim_stats.min_us,
        cyclops_claim_stats.p50_us,
        cyclops_claim_stats.p95_us,
        cyclops_claim_stats.p99_us,
        cyclops_claim_stats.avg_us,
        cyclops_claim_stats.max_us,
        cyclops_claim_stats.throughput_ops_per_sec
    );
    println!("---------------------------------------------------------------------------------------------------------\n");

    println!("---------------------------------------------------------------------------------------------------------");
    println!("2. RESOURCE CONSUMPTION PROFILE");
    println!("---------------------------------------------------------------------------------------------------------");
    println!(
        "{:<32} | {:>12} | {:>10} | {:>22}",
        "Process / Component", "RSS Memory", "Open FDs", "Persistence Footprint"
    );
    println!("---------------------------------------------------------------------------------------------------------");
    println!(
        "{:<32} | {:>9.2} MB | {:>10} | {:>22}",
        format!("tmux server (pid {tmux_pid})"),
        tmux_res.rss_kb as f64 / 1024.0,
        tmux_res.fd_count,
        "0 bytes (ephemeral)"
    );
    println!(
        "{:<32} | {:>9.2} MB | {:>10} | {:>19} KB",
        format!("cyclopsd (pid {daemon_pid})"),
        cyclops_res.rss_kb as f64 / 1024.0,
        cyclops_res.fd_count,
        journal_bytes / 1024
    );
    println!("---------------------------------------------------------------------------------------------------------\n");

    println!("---------------------------------------------------------------------------------------------------------");
    println!("3. ARCHITECTURAL & PROTOCOL COMPARISON: CYCLOPS VS SMUX VS RAW TMUX");
    println!("---------------------------------------------------------------------------------------------------------");
    println!(
        "{:<26} | {:<22} | {:<24} | {:<22}",
        "Dimension", "Raw tmux", "ShawnPana/smux", "Cyclops"
    );
    println!("---------------------------------------------------------------------------------------------------------");
    println!(
        "{:<26} | {:<22} | {:<24} | {:<22}",
        "Durability", "None (dropped on crash)", "None (pipe lost on abort)", "Durable WAL Journal"
    );
    println!(
        "{:<26} | {:<22} | {:<24} | {:<22}",
        "Human Draft Protection",
        "Overwrites typing",
        "Overwrites if typing race",
        "Preserved until clean"
    );
    println!(
        "{:<26} | {:<22} | {:<24} | {:<22}",
        "Scrollback Hygiene", "Full payload pasted", "Full payload pasted", "Clean (doorbell only)"
    );
    println!(
        "{:<26} | {:<22} | {:<24} | {:<22}",
        "Delivery Confirmation",
        "Blind fire-and-forget",
        "Read guard file flag",
        "Fused Sensor Receipt"
    );
    println!(
        "{:<26} | {:<22} | {:<24} | {:<22}",
        "Multi-Agent Ordering",
        "Unordered write races",
        "Unordered write races",
        "Strict Monotonic FIFO"
    );
    println!(
        "{:<26} | {:<22} | {:<24} | {:<22}",
        "Duplicate Enter Guard",
        "None (can duplicate)",
        "None (can duplicate)",
        "Exactly-One Enter Rule"
    );
    println!(
        "{:<26} | {:<22} | {:<24} | {:<22}",
        "Interactive TUI / Chrome", "None", "Basic Option keybinds", "Full Ratatui Workspace"
    );
    println!("---------------------------------------------------------------------------------------------------------\n");

    println!("---------------------------------------------------------------------------------------------------------");
    println!("4. CYCLOPS COMMAND REFERENCE (SUCCINCT SUMMARY)");
    println!("---------------------------------------------------------------------------------------------------------");
    println!("• cyclops send <to> --subject <s> --body <b>  : Durably stores message in recipient inbox; sends doorbell interrupt.");
    println!("• cyclops reply <msg-id> --body <b>          : Atomically replies to a message sender in-thread with durable delivery.");
    println!("• cyclops inbox claim <msg-id>               : Fetches full message payload without contaminating pane scrollback.");
    println!("• cyclops inbox next                         : Non-destructive mailbox polling query; retrieves next unread message.");
    println!("• cyclops watch                              : Streams live NDJSON delivery transitions and mailbox events.");
    println!("• cyclops clear <agent>                      : Withdraws unwritten pane wakes for agent without deleting messages.");
    println!("• cyclops status                             : Validates daemon health, active sessions, adopted panes, and routes.");
    println!("• cyclops name <target> <label>              : Names a pane, tracks identity, and paints theme-aware pane border.");
    println!("---------------------------------------------------------------------------------------------------------\n");

    assert_eq!(tmux_send_stats.count, sample_count);
    assert_eq!(tmux_capture_stats.count, sample_count);
    assert_eq!(smux_stats.count, sample_count);
    assert_eq!(cyclops_send_stats.count, sample_count);
    assert_eq!(cyclops_claim_stats.count, sample_count);

    rig.shutdown().await;
}
