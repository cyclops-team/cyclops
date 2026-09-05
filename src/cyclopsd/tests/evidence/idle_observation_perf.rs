//! Scheduled and release evidence for idle daemon observation work.
//!
//! The test first uses one isolated control fixture to prove every application
//! counter moves after visible cat output. It then starts a fresh isolated
//! screen-tier fixture, resets its counters after attachment, and observes a
//! fixed quiet window. It measures application-level watcher events, recompute
//! starts, and state-observation screen capture requests. It does not claim to
//! count operating-system scheduling wakeups, tmux internals, client refreshes,
//! or terminal-delivery captures.

use crate::common;

use std::time::{Duration, Instant};

use common::{tmux_available, wait_pane_state, Rig, CAT_MANIFEST};
use serde_json::json;

const IDLE_WINDOW: Duration = Duration::from_secs(1);
const POSITIVE_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const POSITIVE_CONTROL_LINE: &str = "idle-observation-positive-control";

/// Wait only for the test's observable counter evidence. This bounded
/// test-side poll does not create daemon work; it is not a timing delay used
/// to make the positive control pass.
async fn wait_for_positive_control(rig: &Rig) -> cyclopsd::ObservationWorkCounts {
    let deadline = Instant::now() + POSITIVE_CONTROL_TIMEOUT;
    loop {
        let counts = rig.daemon.observation_work_counts_for_test();
        if counts.watcher_event_wakes > 0
            && counts.observation_recompute_starts > 0
            && counts.screen_capture_requests > 0
        {
            return counts;
        }
        assert!(
            Instant::now() < deadline,
            "positive output control did not reach every observation counter: {counts:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Retained only in scheduled and release evidence lanes. Run directly with
/// `cargo test -p cyclopsd --test idle_observation_perf -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "scheduled and release idle observation measurement"]
async fn a_stable_screen_tier_pane_starts_no_observation_work_while_idle() {
    if !tmux_available() {
        eprintln!("skipping idle observation measurement: tmux not on PATH");
        return;
    }

    // A literal line makes cat produce a real output event. The separate rig
    // prevents its watcher/debounce cleanup from becoming false work in the
    // quiet-window measurement that follows.
    let positive_control_counts = {
        let mut control = Rig::new(
            "idle-observation-control",
            CAT_MANIFEST,
            "cat",
            "receipt_block_ms = 1000\n",
        )
        .await;
        let pane = control.pane_ids().await[0].clone();
        wait_pane_state(&mut control, "idle").await;
        control.daemon.reset_observation_work_counts_for_test();
        control
            .tmux
            .run_ok(&["send-keys", "-l", "-t", &pane, POSITIVE_CONTROL_LINE]);
        control.tmux.run_ok(&["send-keys", "-t", &pane, "Enter"]);
        control.tmux.wait_screen("main", POSITIVE_CONTROL_LINE);
        let counts = wait_for_positive_control(&control).await;
        control.shutdown().await;
        counts
    };
    assert!(
        positive_control_counts.watcher_event_wakes > 0,
        "cat output must reach the daemon watcher"
    );
    assert!(
        positive_control_counts.observation_recompute_starts > 0,
        "cat output must start a pane observation"
    );
    assert!(
        positive_control_counts.screen_capture_requests > 0,
        "the screen-tier fixture must issue a state-observation capture"
    );

    // The fresh rig repeats only normal attachment. It cannot inherit the
    // control fixture's watcher/debounce cleanup.
    let mut rig = Rig::new(
        "idle-observation-perf",
        CAT_MANIFEST,
        "cat",
        "receipt_block_ms = 1000\n",
    )
    .await;
    wait_pane_state(&mut rig, "idle").await;
    rig.daemon.reset_observation_work_counts_for_test();
    // This is the measured quiet period, not a timing retry: setup above is
    // already synchronized. One second covers the daemon's bounded 300ms
    // output-settle and 100ms lifecycle-recheck timers without generating a
    // new request or pane output.
    tokio::time::sleep(IDLE_WINDOW).await;
    let counts = rig.daemon.observation_work_counts_for_test();
    rig.shutdown().await;

    assert_eq!(
        counts.watcher_event_wakes, 0,
        "a quiet pane must not deliver a watcher event after the reset"
    );
    assert_eq!(
        counts.observation_recompute_starts, 0,
        "a quiet pane must not start another state observation"
    );
    assert_eq!(
        counts.screen_capture_requests, 0,
        "a quiet pane must not request another state-observation capture"
    );

    println!(
        "CYCLOPS_IDLE_OBSERVATION_JSON={}",
        json!({
            "schema": 1,
            "kind": "cyclops_idle_observation_counts",
            "benchmark_test_build_ref": cyclops_proto::BUILD_REF,
            "cyclopsd_version": env!("CARGO_PKG_VERSION"),
            "workload": {
                "fixture": "two sequential isolated tmux servers, each with one screen-tier CAT_MANIFEST pane running cat",
                "positive_control": "one literal line sent to cat in a separate control fixture must reach every application counter",
                "baseline": "the fresh quiet fixture completes attachment and readiness checks before counters reset",
                "idle_window_ms": IDLE_WINDOW.as_millis(),
                "counts": {
                    "watcher_event_wakes": "PaneEvent entries accepted by the daemon after the reset; not operating-system wakeups",
                    "observation_recompute_starts": "pane observation transactions that acquired their route gate after the reset",
                    "screen_capture_requests": "state-observation capture-pane commands issued after the reset",
                },
                "excludes": [
                    "daemon boot and fixture attachment",
                    "the separate positive output control fixture",
                    "client requests after the reset",
                    "agent or human pane output after the reset",
                    "terminal-delivery and composer-recovery captures",
                    "tmux internals and operating-system scheduler wakeups",
                ],
            },
            "positive_control_counts": {
                "watcher_event_wakes": positive_control_counts.watcher_event_wakes,
                "observation_recompute_starts": positive_control_counts.observation_recompute_starts,
                "screen_capture_requests": positive_control_counts.screen_capture_requests,
            },
            "counts": {
                "watcher_event_wakes": counts.watcher_event_wakes,
                "observation_recompute_starts": counts.observation_recompute_starts,
                "screen_capture_requests": counts.screen_capture_requests,
            },
        })
    );
}
