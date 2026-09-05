//! Switching themes while the rig is up.
//!
//! `cyclops theme <name>` writes the config key and asks the daemon to
//! re-read it. Two things have to happen for that to be a switch and not a
//! promise: the borders the daemon owns repaint at once, and the surfaces
//! it does not own get told so they can re-read the selection themselves.
//!
//! Every check asks tmux rather than the daemon. The daemon believing it
//! repainted a border is the thing under test.
//!
//! Same isolated-tmux rig as the m1 tests (tests/common); the bounded
//! waits here are test-side and outside the daemon's zero-polling
//! contract.

use crate::common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

/// Two themes that share no color, so a border can only be wearing one of
/// them. Every token is set: a theme file that is missing one falls back
/// to the compiled table, and a border painted from the table would pass
/// this test while proving nothing about the file.
fn theme_file(role: &str, dim: &str, state: &str) -> String {
    let mut out = String::from("[role]\n");
    for slot in 1..=8 {
        out.push_str(&format!("{slot} = \"{role}\"\n"));
    }
    out.push_str(&format!(
        "[surface]\nfg = \"{dim}\"\ndim = \"{dim}\"\naccent = \"{role}\"\n"
    ));
    out.push_str(&format!("[eye]\ncalm = \"{dim}\"\nalert = \"{role}\"\n"));
    out.push_str("[state]\n");
    for token in ["healthy", "needs_you", "terminal", "quiet", "dead"] {
        out.push_str(&format!("{token} = \"{state}\"\n"));
    }
    out.push_str("[badge]\n");
    for token in ["healthy", "needs_you", "terminal", "quiet"] {
        out.push_str(&format!("{token} = \"{state}\"\n"));
    }
    out
}

/// The raw format string cyclops installed on a pane, styling included.
/// The expansion carries the text; the format carries the color, and the
/// color is the whole question here.
fn border_format(rig: &Rig, pane: &str) -> String {
    let out = rig
        .tmux
        .run(&["show-options", "-p", "-t", pane, "-v", "pane-border-format"]);
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// Bounded test-side wait for a border to carry a color.
fn wait_border(rig: &Rig, pane: &str, needle: &str) -> String {
    let t = Instant::now();
    loop {
        let f = border_format(rig, pane);
        if f.contains(needle) {
            return f;
        }
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "border never carried {needle}: {f:?}"
        );
        // No event: the border is read back from tmux, which the daemon
        // paints without announcing.
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The whole `cyclops theme <name>` path from the daemon's side: the key
/// moves, the daemon is told, and the border a named pane wears is in the
/// new theme before anything else happens on the rig.
///
/// Without the nudge the switch still lands, but only on the next fused
/// state change, which on a calm rig is not a time anyone can name. That
/// gap is what this test exists to close.
#[tokio::test]
async fn a_theme_switch_repaints_live_borders_and_tells_subscribers() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let mut rig = Rig::new("m5theme", CAT_MANIFEST, "cat", "").await;
    std::fs::create_dir_all(rig.home.join("themes")).expect("create themes dir");
    std::fs::write(
        rig.home.join("themes/alpha.toml"),
        theme_file("#010203", "#040506", "#070809"),
    )
    .expect("write alpha");
    std::fs::write(
        rig.home.join("themes/beta.toml"),
        theme_file("#aabbcc", "#ddeeff", "#123456"),
    )
    .expect("write beta");

    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "reviewer").await;

    // Alpha first, so the switch below is theme to theme and not a theme
    // arriving on top of the compiled defaults.
    rig.rewrite_config("theme = \"alpha\"\n");
    let answer = rig.ctl.request("theme.reload", json!({})).await;
    assert_eq!(answer["result"]["theme"], "alpha");
    let f = wait_border(&rig, &pane, "#010203");
    assert!(f.contains("#040506"), "dim is not alpha's: {f:?}");
    assert!(f.contains("#070809"), "state is not alpha's: {f:?}");
    // Subscribers are told, so a running `cyclops ui` wakes and re-reads
    // the selection. The event carries the name and no colors: every
    // surface resolves its own, and one that took a palette off the wire
    // could show a theme no file on this machine holds.
    let ev = rig
        .ev
        .wait_event(Duration::from_secs(5), |v| v["event"] == "theme")
        .await;
    assert_eq!(ev["data"]["name"], "alpha");
    assert!(ev["data"].get("role.1").is_none(), "{ev}");

    // The switch itself. Nothing on the rig moves but the config key.
    rig.rewrite_config("theme = \"beta\"\n");
    let answer = rig.ctl.request("theme.reload", json!({})).await;
    assert_eq!(answer["result"]["theme"], "beta");
    let f = wait_border(&rig, &pane, "#aabbcc");
    assert!(f.contains("#ddeeff"), "dim is not beta's: {f:?}");
    assert!(f.contains("#123456"), "state is not beta's: {f:?}");
    assert!(!f.contains("#010203"), "alpha's role color survived: {f:?}");
    // One event per switch, in order: a subscriber that saw the first one
    // is looking at the second, not at a repeat of the first.
    let ev = rig
        .ev
        .wait_event(Duration::from_secs(5), |v| v["event"] == "theme")
        .await;
    assert_eq!(ev["data"]["name"], "beta");

    rig.shutdown().await;
}

/// A theme file that goes wrong under a running daemon keeps the borders
/// it already painted. The engine owns that rule and pins it token by
/// token; what this pins is that the daemon is on the rule's near side,
/// where a half-written file can reach a real pane.
#[tokio::test]
async fn a_half_written_theme_leaves_the_borders_alone() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let mut rig = Rig::new("m5broken", CAT_MANIFEST, "cat", "").await;
    std::fs::create_dir_all(rig.home.join("themes")).expect("create themes dir");
    let path = rig.home.join("themes/alpha.toml");
    std::fs::write(&path, theme_file("#010203", "#040506", "#070809")).expect("write alpha");

    let pane = rig.pane_ids().await[0].clone();
    rig.label(&pane, "reviewer").await;
    rig.rewrite_config("theme = \"alpha\"\n");
    rig.ctl.request("theme.reload", json!({})).await;
    wait_border(&rig, &pane, "#010203");

    // The shape an editor leaves behind mid-save: valid TOML, most of the
    // palette gone. Loading it would paint the missing tokens out of the
    // compiled table, which is a different palette on the same border.
    std::fs::write(&path, "[surface]\ndim = \"#040506\"\n").expect("truncate alpha");
    let answer = rig.ctl.request("theme.reload", json!({})).await;
    assert_eq!(answer["result"]["theme"], "alpha");
    let f = border_format(&rig, &pane);
    assert!(f.contains("#010203"), "the role color was lost: {f:?}");
    assert!(f.contains("#070809"), "the state color was lost: {f:?}");

    rig.shutdown().await;
}
