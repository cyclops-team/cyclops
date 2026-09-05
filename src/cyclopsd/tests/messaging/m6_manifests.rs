//! The daemon half of the first run.
//!
//! An install is two binaries. The detection manifests reach the machine
//! through `cyclops start`, which writes them into `$CYCLOPS_HOME/manifests`,
//! and cyclopsd has to find them there with nothing in the config pointing
//! at them. When it does not, every pane reads unknown and every message
//! dies in the gate half a minute later, which is exactly what a real
//! first run did before this milestone.
//!
//! No tmux here: both facts are decided at boot, before a session attaches.

use std::path::{Path, PathBuf};

use cyclopsd::Config;
use serde_json::Value;

fn scratch(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(tag);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch home");
    dir
}

/// A minimal manifest: enough to load, not enough to be interesting.
const ONE: &str = r#"
[agent]
id = "fix"
display_name = "Fixture"
process_names = ["cat"]

[[rule]]
id = "always_idle"
state = "idle"
priority = 100
region = "pane_title"
regex = ['^']
"#;

/// Boot a daemon on a config that names one session and nothing else, so
/// the manifest directory is resolved by fallback and not by a config key.
async fn boot_on(home: &Path) -> cyclopsd::Daemon {
    std::fs::write(
        home.join("config.toml"),
        format!(
            "sessions = [\"main\"]\ntmux_socket = \"cyc-m6-{}\"\ntmux_config = \"/dev/null\"\n",
            std::process::id()
        ),
    )
    .expect("write config");
    let (cfg, warnings) = Config::load(home).expect("load config");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(
        cfg.manifest_dir.is_none(),
        "this test is about the fallback, so nothing may point at a directory"
    );
    cyclopsd::boot(cfg).await.expect("boot")
}

fn ledger_lines(home: &Path) -> Vec<Value> {
    let path = home.join("ledger").join("main.ndjson");
    std::fs::read_to_string(path)
        .expect("read the session ledger")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("ledger line parses"))
        .collect()
}

/// The fallback, which is what makes `cyclops start` seeding the home
/// enough. Nothing in the config names a directory; the daemon has to find
/// `$CYCLOPS_HOME/manifests` on its own.
#[tokio::test]
async fn the_home_directory_is_where_the_daemon_looks_with_no_config_key() {
    let home = scratch("cyc-m6-fallback");
    std::fs::create_dir_all(home.join("manifests")).expect("create manifests");
    std::fs::write(home.join("manifests").join("fix.toml"), ONE).expect("write manifest");

    let daemon = boot_on(&home).await;
    let status = daemon.status(false);
    let m = status
        .manifests
        .expect("the daemon reports its manifest set");
    assert_eq!(m.ids, vec!["fix"]);
    assert_eq!(
        m.dir.as_deref(),
        Some(home.join("manifests").display().to_string().as_str()),
        "the daemon read manifests from somewhere other than the home"
    );

    // And a daemon that found some says nothing about not having any.
    let notified = ledger_lines(&home)
        .iter()
        .any(|l| l["subject"] == "no detection manifests");
    assert!(!notified, "a working install must not raise the alarm");

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(&home);
}

/// The reproduction. A daemon with no manifests boots clean, watches, and
/// can deliver nothing, and every step before this one reported success.
/// So it says so somewhere a person will actually be: the record, which
/// `cyclops ui` streams and `cyclops status` reads the same fact off.
/// stderr alone is not that place, because `cyclopsd &` has no reader.
#[tokio::test]
async fn a_daemon_with_no_manifests_puts_the_warning_on_the_record() {
    let home = scratch("cyc-m6-nomanifest");
    let daemon = boot_on(&home).await;

    let status = daemon.status(false);
    let m = status
        .manifests
        .expect("the daemon reports its manifest set");
    assert!(m.ids.is_empty(), "{:?}", m.ids);

    let lines = ledger_lines(&home);
    let ping = lines
        .iter()
        .find(|l| l["subject"] == "no detection manifests")
        .unwrap_or_else(|| {
            panic!("nothing on the record said the install cannot work: {lines:#?}")
        });
    assert_eq!(ping["kind"], "system");
    assert_eq!(ping["to"][0], "admin");
    // Fyi and not action_required, because the ping points at nothing in
    // the attention register: an action-required ping naming no item never
    // leaves the calm view, and "⚠ action required" under a closed eye is
    // the one contradiction the stream may not show. restart_eye.rs is the
    // test that catches it if this level changes.
    assert_eq!(ping["data"]["level"], "fyi");
    let body = ping["body"].as_str().expect("the ping carries the reason");
    assert!(body.contains("every pane reads unknown"), "{body}");
    assert!(body.contains("no message can be delivered"), "{body}");
    assert!(body.contains("cyclops start"), "{body}");

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(&home);
}
