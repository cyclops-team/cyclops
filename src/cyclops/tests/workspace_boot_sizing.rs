//! Real workspace boot sizing against two isolated tmux servers.
//!
//! The outer server supplies the tty that runs bare `cyclops`; the target
//! server holds the nested pane grid the workspace adopts and resizes. This
//! crosses the public binary entrypoint and `cyclops_workspace::run_async`.
//! A target-side tmux hook records the first resize, so a later reconcile
//! cannot repair a bad boot declaration before the assertion observes it.
//! Its sizing assertion does not require a daemon and tolerates daemon-start
//! failure.

#![cfg(feature = "full-ui")]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use cyclops_testrig::{tmux_available, TmuxServer};

struct BootRig {
    home: PathBuf,
    outer: Option<TmuxServer>,
    target: Option<TmuxServer>,
}

impl BootRig {
    fn outer(&self) -> &TmuxServer {
        self.outer.as_ref().expect("outer server is live")
    }

    fn target(&self) -> &TmuxServer {
        self.target.as_ref().expect("target server is live")
    }
}

impl Drop for BootRig {
    fn drop(&mut self) {
        // Stop the tty process before removing its state. Then ask only the
        // daemon selected by this scratch home to stop; no process-name kill
        // can reach another test or the user's daemon.
        drop(self.outer.take());
        let _ = Command::new(env!("CARGO_BIN_EXE_cyclops"))
            .env("CYCLOPS_HOME", &self.home)
            .args(["daemon", "stop"])
            .output();
        drop(self.target.take());
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn window_size(server: &TmuxServer) -> (u16, u16) {
    let output = server.run(&[
        "display-message",
        "-p",
        "-t",
        "demo",
        "#{window_width} #{window_height}",
    ]);
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let cols = fields
        .next()
        .and_then(|field| field.parse().ok())
        .unwrap_or(0);
    let rows = fields
        .next()
        .and_then(|field| field.parse().ok())
        .unwrap_or(0);
    (cols, rows)
}

fn first_resize(server: &TmuxServer) -> Option<(u16, u16)> {
    let output = server.run(&["show-options", "-gv", "@cyclops_test_first_resize"]);
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

#[test]
fn persisted_open_messages_uses_visible_canvas_size_through_real_boot() {
    if !tmux_available() {
        return;
    }

    let home = cyclops_proto::scratch::scratch_dir("boot-open-messages-e2e");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("create scratch home");
    let mut rig = BootRig {
        home: home.clone(),
        outer: Some(TmuxServer::new("boot-open-messages-outer")),
        target: Some(TmuxServer::new("boot-open-messages-target")),
    };

    rig.target().run_ok(&[
        "new-session",
        "-d",
        "-x",
        "100",
        "-y",
        "30",
        "-s",
        "demo",
        "/bin/sh",
    ]);
    rig.target()
        .run_ok(&["split-window", "-h", "-l", "30", "-t", "demo", "/bin/sh"]);
    rig.target()
        .run_ok(&["split-window", "-v", "-l", "5", "-t", "demo", "/bin/sh"]);
    // Preserve the first target-side resize as immutable server state. A
    // process-level test that polls only the final size can miss a wrong boot
    // declaration when the normal reconcile loop repairs it milliseconds
    // later.
    rig.target().run_ok(&[
        "set-hook",
        "-g",
        "after-resize-window",
        "if-shell -F '#{@cyclops_test_first_resize}' '' \"run-shell \\\"tmux set-option -g @cyclops_test_first_resize '#{window_width} #{window_height}'\\\"\"",
    ]);

    std::fs::write(
        home.join("config.toml"),
        format!(
            "sessions = [\"demo\"]\n\
             tmux_socket = \"{}\"\n\
             tmux_config = \"/dev/null\"\n\
             default_workspace = \"demo\"\n\
             [workspace]\n\
             sidebar_visible = false\n\
             messages_visible = true\n\
             messages_width = 24\n\
             tab_bar_visible = true\n\
             motion = false\n",
            rig.target().socket()
        ),
    )
    .expect("write persisted-open config");

    rig.outer().run_ok(&[
        "new-session",
        "-d",
        "-x",
        "100",
        "-y",
        "30",
        "-s",
        "host",
        "/bin/sh",
    ]);
    let output = rig
        .outer()
        .run(&["list-panes", "-t", "host", "-F", "#{pane_id}"]);
    let host_pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let command = format!(
        "export CYCLOPS_HOME='{}'; exec '{}'",
        home.display(),
        env!("CARGO_BIN_EXE_cyclops")
    );
    rig.outer()
        .run_ok(&["send-keys", "-l", "-t", &host_pane, &command]);
    rig.outer()
        .run_ok(&["send-keys", "-t", &host_pane, "Enter"]);

    // The target-side hook is the event this regression protects. Waiting
    // for unrelated screen text made the test depend on renderer and terminal
    // scheduling after the size had already been declared correctly.
    wait_until("the production cold-boot resize", || {
        first_resize(rig.target()).is_some()
    });
    assert_eq!(
        first_resize(rig.target()),
        Some((72, 26)),
        "the first real run_async resize used the local Messages paint canvas"
    );
    assert_eq!(
        window_size(rig.target()),
        (72, 26),
        "the live workspace converged on the same shared tmux geometry"
    );

    // Cleanup runs in Drop on every assertion path. Taking the fields here
    // only makes the success path's order explicit for readers.
    drop(rig.outer.take());
}
