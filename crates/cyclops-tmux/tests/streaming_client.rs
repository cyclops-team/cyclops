//! Streaming control-client acceptance (workspace Step 2).
//!
//! The adapter already ships a long-lived `ControlClient` with a typed
//! notification stream; these tests pin the acceptance criteria the
//! workspace plan names.

mod common;

use std::time::Duration;

use common::TestServer;
use cyclops_tmux::{ControlClient, Notification};

/// Bytes written into a pane arrive as decoded `%output` / `%extended-output`
/// on the control stream.
#[tokio::test]
async fn decoded_echo_arrives_on_the_notification_stream() {
    let Some(srv) = TestServer::new("stream-echo") else {
        return;
    };
    srv.new_session("echo");

    let (client, mut notif) = ControlClient::spawn(srv.config("echo"))
        .await
        .expect("attach");

    srv.tmux_ok(&["send-keys", "-t", "%0", r"printf 'STREAM_HELLO\n'", "Enter"]);

    let mut saw = false;
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(n) = notif.recv().await {
            match n {
                Notification::Output { data, .. } | Notification::ExtendedOutput { data, .. }
                    if data.windows(12).any(|w| w == b"STREAM_HELLO") =>
                {
                    saw = true;
                    return;
                }
                _ => {}
            }
        }
    })
    .await;
    assert!(saw, "decoded pane bytes never arrived on the stream");

    client.shutdown().await;
    assert!(
        srv.socket_path().is_some(),
        "rig server survives client detach"
    );
}

/// Structural mutations on the rig surface as typed notifications in order.
#[tokio::test]
async fn structural_notifications_arrive_for_external_mutations() {
    let Some(srv) = TestServer::new("stream-struct") else {
        return;
    };
    srv.new_session("struct");

    let (client, mut notif) = ControlClient::spawn(srv.config("struct"))
        .await
        .expect("attach");

    srv.tmux_ok(&["new-window", "-t", "struct", "-n", "extra", "/bin/sh"]);

    let mut saw_add = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let Some(n) = tokio::time::timeout(Duration::from_millis(500), notif.recv())
            .await
            .ok()
            .flatten()
        else {
            continue;
        };
        if matches!(n, Notification::WindowAdd { .. }) {
            saw_add = true;
            break;
        }
    }
    assert!(saw_add, "new-window did not surface WindowAdd");

    srv.tmux_ok(&["kill-window", "-t", "struct:extra"]);

    let mut saw_close = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let Some(n) = tokio::time::timeout(Duration::from_millis(500), notif.recv())
            .await
            .ok()
            .flatten()
        else {
            continue;
        };
        if matches!(
            n,
            Notification::WindowClose { .. } | Notification::UnlinkedWindowClose { .. }
        ) {
            saw_close = true;
            break;
        }
    }
    assert!(
        saw_close,
        "kill-window did not surface a window-close notification"
    );

    client.shutdown().await;
}
