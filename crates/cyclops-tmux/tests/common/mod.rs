//! Shared harness for integration tests.
//!
//! Every test runs against its own isolated tmux server: unique `-L` socket
//! (test tag plus pid), `-f /dev/null`, killed on drop. Nothing here can
//! reach the user's default tmux server. Tests skip with an eprintln when
//! no tmux binary is on PATH.

#![allow(dead_code)]

use std::process::{Command, Output};
use std::time::Duration;

use cyclops_tmux::{ControlConfig, PaneEvent};
use tokio::sync::broadcast;

pub struct TestServer {
    pub sock: String,
}

impl TestServer {
    /// None (caller returns early, skipping the test) when tmux is absent.
    pub fn new(tag: &str) -> Option<TestServer> {
        if Command::new("tmux").arg("-V").output().is_err() {
            eprintln!("skipping: no tmux binary on PATH");
            return None;
        }
        Some(TestServer {
            sock: format!("cyclops-test-{tag}-{}", std::process::id()),
        })
    }

    /// Run a tmux command against the isolated server.
    pub fn tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .args(["-L", &self.sock, "-f", "/dev/null"])
            .args(args)
            .env_remove("TMUX")
            .output()
            .expect("run tmux")
    }

    /// Same, but the command must succeed.
    pub fn tmux_ok(&self, args: &[&str]) {
        let out = self.tmux(args);
        assert!(
            out.status.success(),
            "tmux {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Detached session running /bin/sh, fixed 120x30 grid.
    pub fn new_session(&self, name: &str) {
        self.tmux_ok(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "120",
            "-y",
            "30",
            "/bin/sh",
        ]);
    }

    /// Control config attaching to a session on this server.
    pub fn config(&self, session: &str) -> ControlConfig {
        ControlConfig::attach(session)
            .on_socket(self.sock.clone())
            .with_config_file("/dev/null")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.sock, "kill-server"])
            .env_remove("TMUX")
            .output();
    }
}

/// Await the first event matching `pred`, bounded. Panics with the timeout
/// context on expiry so failures are diagnosable.
pub async fn await_event<F>(
    rx: &mut broadcast::Receiver<PaneEvent>,
    what: &str,
    mut pred: F,
) -> PaneEvent
where
    F: FnMut(&PaneEvent) -> bool,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(e) if pred(&e) => return e,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    panic!("event channel closed while waiting for {what}")
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Bounded retry for conditions that have no event to await (file writes
/// from inside a pane, shell startup). Test-only; the product never polls.
pub async fn eventually<F: FnMut() -> bool>(what: &str, mut cond: F) {
    for _ in 0..100 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("condition never became true: {what}");
}
