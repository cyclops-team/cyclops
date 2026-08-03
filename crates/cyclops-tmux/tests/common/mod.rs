//! Shared harness for integration tests.
//!
//! The isolated tmux server and its teardown are `cyclops-testrig`'s, not
//! this file's: one place holds the naming, the isolation flags, and the
//! kill-then-unlink rule. What lives here is only what this crate's tests
//! want on top of it, the fixed-size session and the control config that
//! points at it. Tests skip with an eprintln when no tmux is on PATH.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;

use cyclops_testrig::{tmux_available, TmuxServer};
use cyclops_tmux::{ControlConfig, PaneEvent};
use tokio::sync::broadcast;

pub struct TestServer {
    server: TmuxServer,
}

impl TestServer {
    /// None (caller returns early, skipping the test) when tmux is absent.
    pub fn new(tag: &str) -> Option<TestServer> {
        if !tmux_available() {
            eprintln!("skipping: no tmux binary on PATH");
            return None;
        }
        Some(TestServer {
            server: TmuxServer::new(tag),
        })
    }

    /// The `-L` name, for tests that call tmux code paths themselves.
    pub fn sock(&self) -> &str {
        self.server.socket()
    }

    /// Run a tmux command against the isolated server.
    pub fn tmux(&self, args: &[&str]) -> Output {
        self.server.run(args)
    }

    /// Same, but the command must succeed.
    pub fn tmux_ok(&self, args: &[&str]) {
        self.server.run_ok(args);
    }

    /// Detached session running /bin/sh, fixed 120x30 grid.
    pub fn new_session(&self, name: &str) {
        self.server.run_ok(&[
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
            .on_socket(self.sock().to_string())
            .with_config_file("/dev/null")
    }

    /// Where this server's socket file lives. None when no server is up.
    pub fn socket_path(&self) -> Option<PathBuf> {
        self.server.socket_path()
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
