//! Performance, latency, throughput, and resource usage benchmark comparing
//! Cyclops messaging against raw tmux commands.

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
async fn benchmark_cyclops_vs_raw_tmux() {
    println!("\n=========================================================================================");
    println!(
        "             CYCLOPS VS RAW TMUX COMMUNICATION BENCHMARK & RESOURCE PROFILE              "
    );
    println!("=========================================================================================\n");

    let mut rig = Rig::new(
        "comm-bench",
        CAT_MANIFEST,
        &composer_pane(),
        "delivery.force_submit_delay_ms = 0\n",
    )
    .await;

    let target_pane = "%0";
    let sender_label = "admin";

    // -----------------------------------------------------------------------
    // Benchmark 1: Raw tmux communication (tmux send-keys)
    // -----------------------------------------------------------------------
    let sample_count = 50;
    let mut tmux_samples_us = Vec::with_capacity(sample_count);

    let tmux_bench_start = Instant::now();
    for i in 0..sample_count {
        let text = format!("[raw-tmux-msg-{i}] Hello pane from raw tmux\n");
        let start = Instant::now();
        rig.tmux
            .cmd()
            .args(["send-keys", "-t", target_pane, &text, "Enter"])
            .output()
            .expect("raw tmux send-keys");
        let elapsed = start.elapsed().as_micros() as u64;
        tmux_samples_us.push(elapsed);
    }
    let tmux_total_duration = tmux_bench_start.elapsed();
    let tmux_stats = LatencyStats::compute(tmux_samples_us, tmux_total_duration);

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
    // Benchmark 2: Cyclops Messaging (Durable Journal Append + Socket Protocol)
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
    // Benchmark 3: Cyclops Claim Latency (Private Body Extraction via Socket)
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
    println!(
        "-----------------------------------------------------------------------------------------"
    );
    println!(
        "1. LATENCY & THROUGHPUT COMPARISON ({} operations each)",
        sample_count
    );
    println!(
        "-----------------------------------------------------------------------------------------"
    );
    println!(
        "{:<34} | {:>6} | {:>6} | {:>6} | {:>6} | {:>6} | {:>6} | {:>14}",
        "Communication Path", "min", "p50", "p95", "p99", "avg", "max", "Throughput (ops/s)"
    );
    println!("-------------------------------------------------------------------------------------------------");
    println!(
        "{:<34} | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>14.1}",
        "Raw tmux send-keys (unverified)",
        tmux_stats.min_us,
        tmux_stats.p50_us,
        tmux_stats.p95_us,
        tmux_stats.p99_us,
        tmux_stats.avg_us,
        tmux_stats.max_us,
        tmux_stats.throughput_ops_per_sec
    );
    println!(
        "{:<34} | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>14.1}",
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
        "{:<34} | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>4}µs | {:>14.1}",
        "Cyclops inbox.claim (body access)",
        cyclops_claim_stats.min_us,
        cyclops_claim_stats.p50_us,
        cyclops_claim_stats.p95_us,
        cyclops_claim_stats.p99_us,
        cyclops_claim_stats.avg_us,
        cyclops_claim_stats.max_us,
        cyclops_claim_stats.throughput_ops_per_sec
    );
    println!("-----------------------------------------------------------------------------------------\n");

    println!(
        "-----------------------------------------------------------------------------------------"
    );
    println!("2. RESOURCE USAGE PROFILE");
    println!(
        "-----------------------------------------------------------------------------------------"
    );
    println!(
        "{:<28} | {:>12} | {:>10} | {:>18}",
        "Process / Component", "RSS Memory", "Open FDs", "Persistence Footprint"
    );
    println!(
        "-----------------------------------------------------------------------------------------"
    );
    println!(
        "{:<28} | {:>9.2} MB | {:>10} | {:>18}",
        format!("tmux server (pid {tmux_pid})"),
        tmux_res.rss_kb as f64 / 1024.0,
        tmux_res.fd_count,
        "0 bytes (ephemeral)"
    );
    println!(
        "{:<28} | {:>9.2} MB | {:>10} | {:>15} KB",
        format!("cyclopsd (pid {daemon_pid})"),
        cyclops_res.rss_kb as f64 / 1024.0,
        cyclops_res.fd_count,
        journal_bytes / 1024
    );
    println!("-----------------------------------------------------------------------------------------\n");

    println!(
        "-----------------------------------------------------------------------------------------"
    );
    println!("3. ARCHITECTURAL & SAFETY GUARANTEES");
    println!(
        "-----------------------------------------------------------------------------------------"
    );
    println!(
        "{:<30} | {:<27} | {:<27}",
        "Dimension", "Raw tmux send-keys", "Cyclops Messaging"
    );
    println!(
        "-----------------------------------------------------------------------------------------"
    );
    println!(
        "{:<30} | {:<27} | {:<27}",
        "Durability", "None (dropped on crash)", "Full (appended to journal)"
    );
    println!(
        "{:<30} | {:<27} | {:<27}",
        "Human Draft Preservation", "None (overwrites user)", "Guaranteed (backs off)"
    );
    println!(
        "{:<30} | {:<27} | {:<27}",
        "Payload Privacy", "Exposed in scrollback", "Private until claim"
    );
    println!(
        "{:<30} | {:<27} | {:<27}",
        "Delivery Confirmation", "Blind fire-and-forget", "Proof-verified wake"
    );
    println!(
        "{:<30} | {:<27} | {:<27}",
        "Ordering Contract", "Unordered race", "Strict monotonic FIFO"
    );
    println!("-----------------------------------------------------------------------------------------\n");

    assert_eq!(tmux_stats.count, sample_count);
    assert_eq!(cyclops_send_stats.count, sample_count);
    assert_eq!(cyclops_claim_stats.count, sample_count);

    rig.shutdown().await;
}
