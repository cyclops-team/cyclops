//! Sustained-load and byte-robustness tests for the control client (F22).
//!
//! tmux 3.6a passes pane bytes through %output/%extended-output with octal
//! escaping for control bytes only: raw bytes >= 0x80 arrive verbatim, and
//! a multi-byte UTF-8 character split across two pty reads produces two
//! notification lines that are each invalid UTF-8 on their own (MEASURED,
//! F22). The reader must therefore never require the stream to be UTF-8.
//! These tests drive a busy-TUI simulation (braille glyphs, OSC title
//! churn, split sequences) against a live client and require zero
//! disconnects.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::TestServer;
use cyclops_tmux::{ControlClient, Notification, PaneEvent, SessionWatcher, TmuxError};

/// Busy-TUI simulation: braille spinner title churn via OSC, braille-heavy
/// output lines, and one deliberately split multi-byte character per cycle
/// (two printf writes). Bounded loop so a leaked pane cannot spin forever.
const GENERATOR: &str = r#"i=0; while [ $i -lt 100000 ]; do i=$((i+1)); printf '\033]0;\342\240\213 spin %d\007' "$i"; j=0; while [ $j -lt 40 ]; do printf '\342\240\213\342\240\231\342\240\271\342\240\270\342\240\274\342\240\264\342\240\246\342\240\247 line %d.%d\n' "$i" "$j"; j=$((j+1)); done; printf '\342\240'; printf '\213\n'; sleep 0.01; done"#;

/// Compressed soak length. CYCLOPS_SOAK_SECS extends it (60+ for the full
/// variant); default keeps the suite fast.
fn soak_secs() -> u64 {
    std::env::var("CYCLOPS_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
}

/// Heavy multi-byte output plus concurrent command traffic: the exact mix
/// that dropped the control connection 8x in 80s during the M1 soak. The
/// bar is zero Disconnected events and zero command errors for the whole
/// window.
#[tokio::test]
async fn sustained_load_zero_disconnects() {
    let Some(srv) = TestServer::new("sustload") else {
        return;
    };
    srv.new_session("load");
    srv.tmux_ok(&["send-keys", "-t", "%0", GENERATOR, "Enter"]);

    let w = SessionWatcher::connect(srv.config("load"))
        .await
        .expect("connect");
    let mut rx = w.subscribe();

    let disconnects = Arc::new(AtomicU64::new(0));
    let outputs = Arc::new(AtomicU64::new(0));
    let title_changes = Arc::new(AtomicU64::new(0));
    let counter = {
        let disconnects = Arc::clone(&disconnects);
        let outputs = Arc::clone(&outputs);
        let title_changes = Arc::clone(&title_changes);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(PaneEvent::Disconnected) => {
                        disconnects.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(PaneEvent::OutputActivity { .. }) => {
                        outputs.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(PaneEvent::PaneChanged { .. }) => {
                        title_changes.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    // Command traffic like the daemon's: captures, format expansion, and
    // reconcile-grade list-panes, back to back on the shared connection.
    let client = w.client();
    let deadline = Instant::now() + Duration::from_secs(soak_secs());
    let mut commands = 0u64;
    let mut errors: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        let round: [Result<(), TmuxError>; 3] = [
            client.capture_pane("%0").await.map(|_| ()),
            client.display("%0", "#{pane_title}").await.map(|_| ()),
            client
                .command("list-panes -s -t 'load' -F '#{pane_id}'")
                .await
                .map(|_| ()),
        ];
        for r in round {
            commands += 1;
            if let Err(e) = r {
                errors.push(format!("command {commands}: {e}"));
            }
        }
        if errors.len() > 5 {
            break;
        }
    }

    w.shutdown().await;
    // The watcher still owns the broadcast sender after shutdown, so the
    // counter never sees Closed; stop it directly.
    counter.abort();

    assert_eq!(errors, Vec::<String>::new(), "no command errors under load");
    assert_eq!(
        disconnects.load(Ordering::Relaxed),
        0,
        "zero Disconnected events under sustained load"
    );
    assert!(
        outputs.load(Ordering::Relaxed) > 0,
        "load actually produced output activity"
    );
    assert!(
        title_changes.load(Ordering::Relaxed) > 0,
        "title churn actually reached the subscription path"
    );
    assert!(commands > 100, "command traffic actually ran: {commands}");
}

/// A pane byte that can never be UTF-8 (0xFF) arrives raw on the wire
/// (MEASURED). The connection must survive it and the byte must reach the
/// consumer intact.
#[tokio::test]
async fn invalid_utf8_output_survives_and_is_byte_faithful() {
    let Some(srv) = TestServer::new("rawbyte") else {
        return;
    };
    srv.new_session("raw");

    let (client, mut notif) = ControlClient::spawn(srv.config("raw"))
        .await
        .expect("spawn");

    srv.tmux_ok(&["send-keys", "-t", "%0", r"printf 'X\377Y\n'", "Enter"]);

    // The executed printf is the only source of a raw 0xFF byte; the echoed
    // command line carries it as backslash text.
    let seen = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match notif.recv().await {
                Some(Notification::Output { data, .. })
                | Some(Notification::ExtendedOutput { data, .. }) => {
                    if data.windows(3).any(|w| w == [b'X', 0xFF, b'Y']) {
                        return true;
                    }
                }
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .expect("raw byte notification within 10s");
    assert!(seen, "stream closed before the raw byte arrived");

    // The connection is still alive and correlating.
    let out = client
        .command("display-message -p '#{session_name}'")
        .await
        .expect("command after invalid UTF-8 output");
    assert_eq!(out, vec!["raw".to_string()]);

    client.shutdown().await;
}

/// A multi-byte character split across two pty writes yields two
/// notification lines, each invalid UTF-8 alone (MEASURED). Both fragments
/// must arrive and the connection must stay up.
#[tokio::test]
async fn split_multibyte_sequence_survives() {
    let Some(srv) = TestServer::new("splitseq") else {
        return;
    };
    srv.new_session("split");

    let (client, mut notif) = ControlClient::spawn(srv.config("split"))
        .await
        .expect("spawn");

    // Braille U+280B = E2 A0 8B, split 2+1 across writes 400ms apart.
    srv.tmux_ok(&[
        "send-keys",
        "-t",
        "%0",
        r"printf '\342\240'; sleep 0.4; printf '\213\n'",
        "Enter",
    ]);

    let mut collected: Vec<u8> = Vec::new();
    let complete = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match notif.recv().await {
                Some(Notification::Output { data, .. })
                | Some(Notification::ExtendedOutput { data, .. }) => {
                    collected.extend_from_slice(&data);
                    if collected.windows(3).any(|w| w == [0xE2, 0xA0, 0x8B]) {
                        return true;
                    }
                }
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await
    .expect("split sequence within 10s");
    assert!(complete, "stream closed before both fragments arrived");

    let out = client
        .command("display-message -p '#{session_name}'")
        .await
        .expect("command after split sequence");
    assert_eq!(out, vec!["split".to_string()]);

    client.shutdown().await;
}

/// A reply slower than the command timeout is a command failure, not
/// connection death: the late reply is consumed in FIFO order and the next
/// command correlates correctly on the same connection.
///
/// Control mode closes reply blocks for blocking commands (wait-for,
/// run-shell) immediately (MEASURED on 3.6a), so the only way to delay a
/// reply deterministically is to stop the isolated server itself around
/// the command.
#[tokio::test]
async fn timeout_is_command_failure_not_teardown() {
    let Some(srv) = TestServer::new("cmdtimeout") else {
        return;
    };
    srv.new_session("slow");

    let cfg = srv
        .config("slow")
        .with_command_timeout(Duration::from_millis(200));
    let (client, mut notif) = ControlClient::spawn(cfg).await.expect("spawn");

    // Server pid of the isolated server this test owns.
    let pid = client
        .command("display-message -p '#{pid}'")
        .await
        .expect("server pid")
        .join("");
    let pid: i32 = pid.trim().parse().expect("numeric server pid");

    // Freeze the server: the next reply cannot arrive until it resumes.
    assert!(std::process::Command::new("kill")
        .args(["-STOP", &pid.to_string()])
        .status()
        .expect("send SIGSTOP")
        .success());
    let err = client
        .command("display-message -p frozen")
        .await
        .expect_err("no reply from a stopped server within 200ms");
    assert!(std::process::Command::new("kill")
        .args(["-CONT", &pid.to_string()])
        .status()
        .expect("send SIGCONT")
        .success());
    assert!(
        matches!(err, TmuxError::Timeout(_)),
        "expected Timeout, got {err:?}"
    );

    // Let the late reply drain into the abandoned slot in FIFO order.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let out = client
        .command("display-message -p '#{session_name}'")
        .await
        .expect("connection alive after a timed-out command");
    assert_eq!(out, vec!["slow".to_string()], "reply correlation intact");

    // The notification stream never closed.
    assert!(
        !matches!(
            notif.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ),
        "notification stream still open"
    );

    client.shutdown().await;
}
