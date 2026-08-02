//! capture-pane and the load-buffer/paste-buffer round trip.
//!
//! The paste test proves byte-exact delivery: the pane runs `cat > file`
//! with bracketed paste mode enabled, so the file must contain exactly
//! open-marker + payload + close-marker + newline. Any corruption anywhere
//! in load-buffer, the temp file, paste-buffer, or quoting breaks the byte
//! comparison.

mod common;

use std::time::Duration;

use common::{eventually, TestServer};
use cyclops_tmux::ControlClient;

#[tokio::test]
async fn capture_pane_returns_visible_content() {
    let Some(srv) = TestServer::new("capture") else {
        return;
    };
    srv.new_session("cap");
    let (client, _notif) = ControlClient::spawn(srv.config("cap"))
        .await
        .expect("spawn");

    client
        .send_keys("%0", &["echo CYCAP_MARKER_42", "Enter"])
        .await
        .expect("send_keys");

    let mut seen = String::new();
    for _ in 0..100 {
        seen = client.capture_pane("%0").await.expect("capture");
        if seen.contains("CYCAP_MARKER_42") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        seen.contains("CYCAP_MARKER_42"),
        "visible grid should contain the marker: {seen:?}"
    );

    // History capture includes the visible tail too.
    let hist = client
        .capture_pane_history("%0", 50)
        .await
        .expect("capture history");
    assert!(hist.contains("CYCAP_MARKER_42"));

    client.shutdown().await;
}

#[tokio::test]
async fn bracketed_paste_round_trip_is_byte_exact() {
    let Some(srv) = TestServer::new("paste") else {
        return;
    };
    srv.new_session("pa");
    let (client, _notif) = ControlClient::spawn(srv.config("pa")).await.expect("spawn");

    let out_path = cyclops_proto::scratch::scratch_root()
        .join(format!("cyclops-paste-test-{}.bin", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&out_path);

    // Enable bracketed paste in the pane (paste-buffer -p only inserts the
    // markers when the application asked for them), then sink stdin to a
    // file. printf uses octal, POSIX printf has no \x.
    client
        .send_keys(
            "%0",
            &[&format!("printf '\\033[?2004h'; cat > {out_path}"), "Enter"],
        )
        .await
        .expect("start cat");

    // Wait until cat owns the pane before pasting at it.
    let mut cmd = String::new();
    for _ in 0..100 {
        cmd = client
            .display("%0", "#{pane_current_command}")
            .await
            .expect("display");
        if cmd == "cat" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(cmd, "cat", "cat should be running in the pane");

    // Hostile payload: quotes, backslash, tab, unicode, embedded newline.
    let payload: &[u8] =
        "line1 'quoted' \\back\\slash \"double\"\nline2\ttab \u{1F441} end".as_bytes();
    let buf_name = format!("cybuf{}", std::process::id());

    client
        .load_buffer(&buf_name, payload)
        .await
        .expect("load_buffer");
    client
        .paste_buffer(&buf_name, "%0", true, true)
        .await
        .expect("paste_buffer");
    // Newline flushes cat's last line, C-d at line start ends it.
    client
        .send_keys("%0", &["Enter", "C-d"])
        .await
        .expect("finish cat");

    // Expected bytes: bracket-open + payload + bracket-close + the newline
    // from our Enter. paste-buffer converts buffer LF to CR on the wire and
    // the tty's ICRNL converts it back, so the payload newline survives
    // byte-exact.
    let mut expected = Vec::new();
    expected.extend_from_slice(b"\x1b[200~");
    expected.extend_from_slice(payload);
    expected.extend_from_slice(b"\x1b[201~");
    expected.push(b'\n');

    let path = out_path.clone();
    eventually("pasted file matches payload", move || {
        std::fs::read(&path)
            .map(|got| got == expected)
            .unwrap_or(false)
    })
    .await;

    // -d deleted the buffer after pasting.
    let buffers = client.command("list-buffers").await.expect("list-buffers");
    assert!(
        !buffers.iter().any(|l| l.contains(&buf_name)),
        "paste_buffer delete=true should remove the buffer: {buffers:?}"
    );

    let _ = std::fs::remove_file(&out_path);
    client.shutdown().await;
}
