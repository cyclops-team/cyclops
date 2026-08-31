//! Scheduled and release performance evidence for durable mailbox replay.
//!
//! This is deliberately an in-process daemon boot measurement. It starts a
//! fresh daemon over the same isolated tmux server and state root, then proves
//! the replayed body-free snapshot still sees every seeded message. It does not
//! claim to measure executable-process launch, client connection, or terminal
//! notification latency.

mod common;

use std::fs;
use std::path::Path;
use std::time::Instant;

use common::{composer_pane, tmux_available, Rig, CAT_MANIFEST};
use cyclops_proto::MsgSendParams;
use serde_json::{json, Value};

const MESSAGE_COUNTS: [u64; 3] = [0, 1_000, 10_000];
const SAMPLES_PER_COUNT: usize = 3;

#[derive(Clone, Copy)]
struct JournalStats {
    bytes: u64,
    lines: u64,
}

fn workspace_journal_stats(home: &Path) -> JournalStats {
    let Some(path) = fs::read_dir(home.join("workspaces"))
        .expect("workspace directory")
        .find_map(|entry| {
            let path = entry.ok()?.path().join("messages.ndjson");
            path.is_file().then_some(path)
        })
    else {
        return JournalStats { bytes: 0, lines: 0 };
    };
    let bytes = fs::read(path).expect("workspace journal readable");
    JournalStats {
        bytes: bytes.len().try_into().expect("journal fits u64"),
        lines: bytes
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            .try_into()
            .expect("journal line count fits u64"),
    }
}

async fn boot_and_measure(home: &Path) -> (cyclopsd::Daemon, u64) {
    let (config, _) = cyclopsd::Config::load(home).expect("config loads");
    let started = Instant::now();
    let daemon = cyclopsd::boot(config).await.expect("daemon reboots");
    (
        daemon,
        started
            .elapsed()
            .as_micros()
            .try_into()
            .expect("boot duration fits u64"),
    )
}

fn summary(samples: &[u64]) -> Value {
    assert!(!samples.is_empty(), "at least one boot sample");
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let percentile = |numerator: usize| {
        let index = (sorted.len() * numerator).div_ceil(100).saturating_sub(1);
        sorted[index]
    };
    json!({
        "unit": "microseconds",
        "samples": samples,
        "sample_count": samples.len(),
        "p50": percentile(50),
        "p95": percentile(95),
        "max": *sorted.last().expect("nonempty samples"),
    })
}

async fn seed_messages(daemon: &cyclopsd::Daemon, start: u64, end: u64) {
    for index in start..end {
        let accepted = daemon
            .msg_send(
                "admin",
                serde_json::from_value::<MsgSendParams>(json!({
                    "to": ["admin"],
                    "subject": "cold-start replay fixture",
                    "body": "b",
                    "fyi": true,
                    "client_key": format!("cold-start-replay-{index}"),
                }))
                .expect("fixture params"),
            )
            .await
            .expect("fixture message accepted");
        assert_eq!(
            accepted["inserted"], true,
            "fixture message was not inserted"
        );
    }
}

fn assert_replayed_message_count(daemon: &cyclopsd::Daemon, expected: u64) {
    let snapshot = daemon
        .messages_snapshot_for_test("admin", 0)
        .expect("body-free snapshot after replay");
    assert_eq!(
        snapshot.counts.visible_messages, expected,
        "replayed snapshot must retain every seeded message"
    );
}

/// Retained only in the scheduled and release performance lanes. The output
/// joins `scripts/ci-performance.py` metadata, which identifies the exact
/// commit, environment, command, and package version for every saved run. A
/// direct run may skip when tmux is unavailable; the evidence runner rejects
/// that missing JSON report rather than retaining it as a successful measure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "scheduled and release daemon cold-boot/replay measurement"]
async fn daemon_cold_boot_replays_growing_workspace_journals() {
    if !tmux_available() {
        eprintln!("skipping daemon cold-start replay measurement: tmux not on PATH");
        return;
    }

    let rig = Rig::new(
        "cold-start-replay-perf",
        CAT_MANIFEST,
        &composer_pane(),
        "receipt_block_ms = 1000\n",
    )
    .await;
    rig.daemon.shutdown().await;

    let mut measurements = Vec::with_capacity(MESSAGE_COUNTS.len());
    for (position, message_count) in MESSAGE_COUNTS.into_iter().enumerate() {
        let journal = workspace_journal_stats(&rig.home);
        let mut samples = Vec::with_capacity(SAMPLES_PER_COUNT);
        let mut last_daemon = None;
        for sample in 0..SAMPLES_PER_COUNT {
            let (daemon, elapsed) = boot_and_measure(&rig.home).await;
            assert_replayed_message_count(&daemon, message_count);
            samples.push(elapsed);
            if sample + 1 == SAMPLES_PER_COUNT {
                last_daemon = Some(daemon);
            } else {
                daemon.shutdown().await;
            }
        }
        measurements.push(json!({
            "accepted_message_count": message_count,
            "workspace_journal": {
                "bytes": journal.bytes,
                "lines": journal.lines,
            },
            "daemon_boot": summary(&samples),
        }));

        let daemon = last_daemon.expect("last boot stays alive to seed the next workload");
        if let Some(next_message_count) = MESSAGE_COUNTS.get(position + 1) {
            seed_messages(&daemon, message_count, *next_message_count).await;
        }
        daemon.shutdown().await;
    }

    println!(
        "CYCLOPS_DAEMON_COLD_START_REPLAY_JSON={}",
        json!({
            "schema": 1,
            "kind": "cyclops_daemon_cold_start_replay",
            "benchmark_test_build_ref": cyclops_proto::BUILD_REF,
            "cyclopsd_version": env!("CARGO_PKG_VERSION"),
            "workload": {
                "fixture": "operator-addressed FYI messages accepted through WorkspaceMessaging",
                "measurement": "cyclopsd::boot from an already-validated config after clean daemon shutdown",
                "excludes": [
                    "config parsing",
                    "executable-process launch",
                    "client connection",
                    "terminal notification latency",
                    "post-boot snapshot verification",
                ],
                "replay_validation": "a body-free snapshot sees every seeded message after every timed boot",
                "samples_per_message_count": SAMPLES_PER_COUNT,
            },
            "measurements": measurements,
        })
    );
}
