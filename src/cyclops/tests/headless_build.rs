//! Entry-point behavior that exists only in the headless build.

#![cfg(not(feature = "full-ui"))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn scratch_home(tag: &str) -> PathBuf {
    let home = cyclops_proto::scratch::scratch_dir(&format!("headless-{tag}"));
    let _ = std::fs::remove_dir_all(&home);
    home
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .args(args)
        .output()
        .expect("run headless cyclops")
}

#[test]
fn bare_cyclops_names_the_missing_workspace_without_writing_state() {
    let home = scratch_home("bare");
    let output = run(&home, &[]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "the full-screen workspace is not included in this build. Run cyclops --help for headless commands, or install a full Cyclops build."
    );
    assert!(!home.exists(), "a refused workspace created Cyclops state");
}

#[test]
fn interactive_watch_names_the_available_machine_stream() {
    let home = scratch_home("watch");
    let args = &["watch", "--plain"];
    let output = run(&home, args);
    assert_eq!(output.status.code(), Some(2), "args={args:?}");
    assert!(output.stdout.is_empty(), "args={args:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "interactive watch is not included in this build. Use cyclops watch --json for the headless event stream, or install a full Cyclops build.",
        "args={args:?}"
    );
    assert!(
        !home.exists(),
        "a refused interactive UI created Cyclops state"
    );
}

#[test]
fn headless_help_does_not_promise_an_interactive_ui() {
    let home = scratch_home("help");
    let output = run(&home, &["--help"]);

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("This build includes command-line and JSON operation"),
        "{help}"
    );
    assert!(
        help.contains("Interactive watch is not included in this build"),
        "{help}"
    );
    assert!(
        !help.contains("With no command, opens the full-screen workspace"),
        "{help}"
    );
}

#[test]
fn headless_discovery_keeps_the_everyday_front_door_and_advanced_spellings() {
    let home = scratch_home("discovery");
    let help = run(&home, &["--help"]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    let help = String::from_utf8_lossy(&help.stdout);
    for command in ["send", "inbox", "reply", "status", "health", "commands"] {
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with(command)),
            "missing {command:?} in {help}"
        );
    }
    for command in ["workspace", "history", "daemon"] {
        assert!(
            !help
                .lines()
                .any(|line| line.trim_start().starts_with(command)),
            "advanced command {command:?} leaked into {help}"
        );
    }

    let catalog = run(&home, &["commands"]);
    assert!(catalog.status.success());
    assert!(catalog.stderr.is_empty());
    let catalog = String::from_utf8_lossy(&catalog.stdout);
    assert!(catalog.contains("Everyday\n"), "{catalog}");
    assert!(catalog.contains("Workspace\n"), "{catalog}");
    assert!(
        catalog.contains("Diagnosis and compatibility\n"),
        "{catalog}"
    );
    assert!(catalog.contains("history"), "{catalog}");

    let history_help = run(&home, &["help", "history"]);
    assert!(history_help.status.success());
    assert!(history_help.stderr.is_empty());
    let history_help = String::from_utf8_lossy(&history_help.stdout);
    assert!(
        history_help.contains("Usage: cyclops history"),
        "{history_help}"
    );
    assert!(
        !home.exists(),
        "read-only command discovery created Cyclops state"
    );
}
