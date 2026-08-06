//! Concurrent pane hydration tests (task D2).
//!
//! `ControlClient::hydrate_panes` runs each pane's own
//! capture -> capture -> metadata sequence exactly as
//! [`ControlClient::hydrate_pane`] runs it alone, but overlaps independent
//! panes instead of looping serially — today's shape in
//! `crates/cyclops-workspace/src/sync.rs`'s `hydrate_visible_tab`.

mod common;

use std::time::Instant;

use common::TestServer;
use cyclops_tmux::ControlClient;

fn pane_ids(srv: &TestServer, target: &str) -> Vec<String> {
    let out = srv.tmux(&["list-panes", "-t", target, "-F", "#{pane_id}"]);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn hydrate_panes_returns_each_panes_own_content_in_input_order() {
    let Some(srv) = TestServer::new("hyd-conc") else {
        return;
    };
    srv.new_session("conc");
    for _ in 0..3 {
        srv.tmux_ok(&["split-window", "-t", "conc", "/bin/sh"]);
    }
    let ids = pane_ids(&srv, "conc");
    assert_eq!(ids.len(), 4, "expected four panes");

    // Give every pane distinct, identifiable content before hydrating.
    for (i, pane_id) in ids.iter().enumerate() {
        srv.tmux_ok(&[
            "send-keys",
            "-t",
            pane_id,
            &format!("printf 'PANE_{i}_MARKER\\n'"),
            "Enter",
        ]);
    }
    for (i, pane_id) in ids.iter().enumerate() {
        let marker = format!("PANE_{i}_MARKER").into_bytes();
        common::eventually(&format!("pane {i} marker"), || {
            srv.tmux(&["capture-pane", "-p", "-t", pane_id])
                .stdout
                .windows(marker.len())
                .any(|w| w == marker.as_slice())
        })
        .await;
    }

    let (client, _notif) = ControlClient::spawn(srv.config("conc"))
        .await
        .expect("attach");
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let results = client.hydrate_panes(&refs).await;
    assert_eq!(
        results.len(),
        ids.len(),
        "one result per input pane, in order"
    );

    for (i, result) in results.iter().enumerate() {
        let bundle = result
            .as_ref()
            .unwrap_or_else(|e| panic!("pane {i} ({}) failed to hydrate: {e}", ids[i]));
        let visible = String::from_utf8_lossy(&bundle.visible_escaped);
        let marker = format!("PANE_{i}_MARKER");
        assert!(
            visible.contains(&marker),
            "bundle at slot {i} (pane {}) is missing its own marker; \
             a mixed-up slot means concurrent hydration crossed pane content. visible={visible:?}",
            ids[i]
        );
        assert!(bundle.cols > 0 && bundle.rows > 0);
    }

    client.shutdown().await;
}

#[tokio::test]
async fn a_dead_pane_id_fails_only_its_own_slot() {
    let Some(srv) = TestServer::new("hyd-conc-dead") else {
        return;
    };
    srv.new_session("deadmix");
    srv.tmux_ok(&["send-keys", "-t", "%0", "printf 'ALIVE_MARKER\\n'", "Enter"]);
    common::eventually("alive marker", || {
        srv.tmux(&["capture-pane", "-p", "-t", "%0"])
            .stdout
            .windows(12)
            .any(|w| w == b"ALIVE_MARKER")
    })
    .await;

    let (client, _notif) = ControlClient::spawn(srv.config("deadmix"))
        .await
        .expect("attach");
    // %99 never existed on this server.
    let targets = ["%0", "%99"];
    let results = client.hydrate_panes(&targets).await;
    assert_eq!(results.len(), 2);
    assert!(
        results[0].is_ok(),
        "the live pane must still hydrate: {:?}",
        results[0]
    );
    assert!(
        results[1].is_err(),
        "a nonexistent pane id must fail its own slot, not panic or vanish"
    );

    client.shutdown().await;
}

/// Timing PRINT only — recorded, never gated (see
/// `crates/cyclops-workspace/tests/baseline.rs`'s rationale).
#[tokio::test]
async fn timing_serial_vs_concurrent_hydration_on_eight_panes() {
    let Some(srv) = TestServer::new("hyd-timing") else {
        return;
    };
    // A wider/taller grid than TestServer::new_session's fixed 120x30: eight
    // alternating splits run out of room on a small grid ("no space for a
    // new pane"), the same reason
    // crates/cyclops-workspace/tests/baseline.rs's own 8-pane fixture uses
    // 220x50.
    srv.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "timing",
        "-x",
        "220",
        "-y",
        "50",
        "/bin/sh",
    ]);
    for i in 1..8 {
        let dir = if i % 2 == 0 { "-h" } else { "-v" };
        srv.tmux_ok(&["split-window", dir, "-t", "timing", "/bin/sh"]);
    }
    let ids = pane_ids(&srv, "timing");
    assert_eq!(ids.len(), 8, "expected eight panes");

    let (client, _notif) = ControlClient::spawn(srv.config("timing"))
        .await
        .expect("attach");

    // 1. Serial: today's hydrate_visible_tab shape — one hydrate_pane after
    //    another.
    let t = Instant::now();
    for pane_id in &ids {
        client
            .hydrate_pane(pane_id)
            .await
            .expect("serial hydrate_pane");
    }
    let serial = t.elapsed();

    // 2. Concurrent: D2's batch primitive.
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let t = Instant::now();
    let results = client.hydrate_panes(&refs).await;
    let concurrent = t.elapsed();
    for (i, r) in results.iter().enumerate() {
        r.as_ref()
            .unwrap_or_else(|e| panic!("concurrent hydrate of pane {i} failed: {e}"));
    }

    println!("=== D2: serial hydrate_pane vs concurrent hydrate_panes, 8 panes ===");
    println!(
        "serial={:.2}ms concurrent={:.2}ms",
        serial.as_secs_f64() * 1000.0,
        concurrent.as_secs_f64() * 1000.0
    );

    client.shutdown().await;
}
