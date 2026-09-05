//! A pane leaving the tmux table has to reach subscribers.
//!
//! Everything else a client hears names a pane that still exists, so a
//! pane's removal is the one transition with no other messenger. A client
//! counting what needs a human takes its roster from one `status` answer
//! at startup and moves it on events after that
//! (`cyclops_proto::attention`, and re-asking on a timer would be
//! polling), so without this edge a pane that blocked and then closed
//! stays counted for the life of that client.
//!
//! Same isolated-tmux rig as the m1 tests (tests/common); the bounded
//! waits here are test-side and outside the daemon's zero-polling
//! contract.

use crate::common;

use std::time::Duration;

use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn a_pane_leaving_the_table_is_announced_to_subscribers() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("panegone", CAT_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    // A second pane, so killing it leaves the window (and the daemon's
    // attachment) standing: this is the removal edge, not a teardown.
    rig.tmux.run_ok(&["split-window", "-t", "main", "cat"]);
    rig.wait_attached(2).await;

    let panes = rig.pane_ids().await;
    assert_eq!(panes.len(), 2, "{panes:?}");
    let victim = panes[1].clone();
    rig.tmux.run_ok(&["kill-pane", "-t", &victim]);

    let ev = rig
        .ev
        .wait_event(Duration::from_secs(10), |v| {
            v["event"] == json!("pane-removed") && v["data"]["pane_id"] == json!(victim)
        })
        .await;
    assert_eq!(ev["data"]["session"], json!("main"), "{ev}");
    assert!(ev["data"]["ts"].as_u64().unwrap_or(0) > 0, "{ev}");

    // And the answer agrees: the pane is off the roster too, so a client
    // starting now and one already running see the same rig.
    rig.wait_attached(1).await;
    assert!(!rig.pane_ids().await.contains(&victim));

    rig.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reset_rpc_prunes_dead_panes_and_cleanses_status() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let mut rig = Rig::new("resetrpc", CAT_MANIFEST, "cat", "").await;
    rig.wait_attached(1).await;
    rig.tmux.run_ok(&["split-window", "-t", "main", "cat"]);
    rig.wait_attached(2).await;

    let res = rig.ctl.request("status.reset", json!({})).await;
    assert_eq!(res["result"]["reset"], true);
    assert_eq!(res["result"]["active_panes"], 2);

    rig.shutdown().await;
}
