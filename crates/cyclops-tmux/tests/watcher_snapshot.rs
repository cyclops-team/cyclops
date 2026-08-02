//! Bootstrap snapshot correctness on an isolated server.

mod common;

use common::TestServer;
use cyclops_tmux::SessionWatcher;

#[tokio::test]
async fn snapshot_sees_created_panes_with_ids_titles_sizes() {
    let Some(srv) = TestServer::new("snapshot") else {
        return;
    };
    srv.new_session("snap");
    srv.tmux_ok(&["split-window", "-t", "snap", "/bin/sh"]);
    srv.tmux_ok(&["select-pane", "-t", "%0", "-T", "alpha"]);
    srv.tmux_ok(&["select-pane", "-t", "%1", "-T", "beta"]);

    let w = SessionWatcher::connect(srv.config("snap"))
        .await
        .expect("connect");
    let rows = w.snapshot();
    assert_eq!(rows.len(), 2, "expected both panes: {rows:?}");

    assert_eq!(rows[0].pane_id, "%0");
    assert_eq!(rows[1].pane_id, "%1");
    assert_eq!(rows[0].title, "alpha");
    assert_eq!(rows[1].title, "beta");
    for r in &rows {
        assert_eq!(r.width, 120, "full-width panes in a vertical split");
        // macOS reports /bin/sh panes as "sh" or "bash" depending on timing.
        assert!(!r.current_command.is_empty());
        assert!(!r.dead);
        assert!(!r.in_mode);
        assert_eq!(r.window_id, "@0");
    }
    // 30 rows minus one divider split across two panes.
    assert_eq!(rows[0].height + rows[1].height, 29);
    assert_eq!(rows.iter().filter(|r| r.active).count(), 1);

    // pane_pid is the pane's root process: live, and unique per pane.
    for r in &rows {
        assert!(r.pane_pid > 0, "pane_pid must be a real pid: {r:?}");
    }
    assert_ne!(rows[0].pane_pid, rows[1].pane_pid);

    // pane() agrees with snapshot().
    assert_eq!(w.pane("%1").unwrap().title, "beta");
    assert!(w.pane("%9").is_none());

    w.shutdown().await;
}
