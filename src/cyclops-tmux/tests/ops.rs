//! Structural operations against an isolated tmux server.
//!
//! Each test asserts what tmux ended up holding, read back with the rig's
//! own `tmux` invocations rather than with the operation under test, so a
//! command that is spelled wrong cannot also mis-read its own result.

mod common;

use std::path::{Path, PathBuf};

use common::TestServer;
use cyclops_tmux::{ControlClient, ControlConfig, PaneDirection, SplitDirection, TmuxError};

/// Pane ids of a target, in layout order.
fn pane_ids(srv: &TestServer, target: &str) -> Vec<String> {
    lines(srv, &["list-panes", "-t", target, "-F", "#{pane_id}"])
}

/// Window names of a session, in index order.
fn window_names(srv: &TestServer, session: &str) -> Vec<String> {
    lines(
        srv,
        &["list-windows", "-t", session, "-F", "#{window_name}"],
    )
}

fn lines(srv: &TestServer, args: &[&str]) -> Vec<String> {
    let out = srv.tmux(args);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// One format expanded against one target.
fn field(srv: &TestServer, target: &str, format: &str) -> String {
    let out = srv.tmux(&["display-message", "-p", "-t", target, format]);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `field`, but for a format tmux fills in a moment after it answers the
/// command that created the target.
///
/// `#{pane_current_path}` is the one observed doing it: tmux replies to
/// `new-window` with the window id before the pane's process has a working
/// directory to report, so an immediate read can come back empty. That is
/// not a Cyclops behavior to assert against, it is a read taken too early,
/// and it made this test fail once on the tmux-head job and pass on the
/// next run with no change in between.
///
/// Bounded, and it returns whatever it last saw rather than panicking, so
/// a genuinely empty field still fails at the caller's assertion with the
/// caller's message instead of a timeout here.
fn field_when_set(srv: &TestServer, target: &str, format: &str) -> String {
    let mut last = String::new();
    for _ in 0..50 {
        last = field(srv, target, format);
        if !last.is_empty() {
            return last;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    last
}

/// A pane's geometry: (left, top, width).
fn geometry(srv: &TestServer, pane: &str) -> (u16, u16, u16) {
    let raw = field(srv, pane, "#{pane_left}\t#{pane_top}\t#{pane_width}");
    let mut f = raw.split('\t').map(|n| n.parse::<u16>().expect("number"));
    (
        f.next().expect("left"),
        f.next().expect("top"),
        f.next().expect("width"),
    )
}

/// A scratch directory that exists, cleaned first so a reused pid cannot
/// inherit one.
fn scratch(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(tag);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn same_dir(a: &str, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[tokio::test]
async fn select_pane_toward_walks_left_and_right() {
    let Some(srv) = TestServer::new("ops-select-lr") else {
        return;
    };
    srv.new_session("s");
    srv.tmux_ok(&["split-window", "-h", "-t", "s"]);
    let panes = pane_ids(&srv, "s");
    let (left, right) = (&panes[0], &panes[1]);
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client
        .select_pane_toward(PaneDirection::Left)
        .await
        .expect("left");
    assert_eq!(&field(&srv, "s", "#{pane_id}"), left);
    client
        .select_pane_toward(PaneDirection::Right)
        .await
        .expect("right");
    assert_eq!(&field(&srv, "s", "#{pane_id}"), right);

    client.shutdown().await;
}

#[tokio::test]
async fn select_pane_toward_walks_up_and_down() {
    let Some(srv) = TestServer::new("ops-select-ud") else {
        return;
    };
    srv.new_session("s");
    srv.tmux_ok(&["split-window", "-v", "-t", "s"]);
    let panes = pane_ids(&srv, "s");
    let (top, bottom) = (&panes[0], &panes[1]);
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client
        .select_pane_toward(PaneDirection::Up)
        .await
        .expect("up");
    assert_eq!(&field(&srv, "s", "#{pane_id}"), top);
    client
        .select_pane_toward(PaneDirection::Down)
        .await
        .expect("down");
    assert_eq!(&field(&srv, "s", "#{pane_id}"), bottom);

    client.shutdown().await;
}

#[tokio::test]
async fn select_pane_by_id_focuses_that_pane() {
    let Some(srv) = TestServer::new("ops-select-id") else {
        return;
    };
    srv.new_session("s");
    srv.tmux_ok(&["split-window", "-h", "-t", "s"]);
    let first = pane_ids(&srv, "s")[0].clone();
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client.select_pane(&first).await.expect("select");

    assert_eq!(field(&srv, "s", "#{pane_id}"), first);
    client.shutdown().await;
}

#[tokio::test]
async fn source_targeted_direction_does_not_borrow_the_ambient_current_pane() {
    let Some(srv) = TestServer::new("ops-select-exact-source") else {
        return;
    };
    srv.new_session("s");
    srv.tmux_ok(&["split-window", "-h", "-t", "s"]);
    srv.tmux_ok(&["split-window", "-h", "-t", "s"]);
    let panes = pane_ids(&srv, "s");
    let (left, middle, right) = (&panes[0], &panes[1], &panes[2]);
    srv.tmux_ok(&["select-pane", "-t", right]);
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client
        .select_pane_toward_from(middle, PaneDirection::Left)
        .await
        .expect("select left of the captured source");

    assert_eq!(field(&srv, "s", "#{pane_id}"), *left);
    client.shutdown().await;
}

#[tokio::test]
async fn exact_focus_routes_stay_on_the_configured_server_and_config() {
    let Some(ambient) = TestServer::new("ops-focus-route-ambient") else {
        return;
    };
    let Some(configured) = TestServer::new("ops-focus-route-configured") else {
        return;
    };
    ambient.new_session("same-name");
    ambient.tmux_ok(&["split-window", "-h", "-t", "same-name"]);
    let ambient_before = field(&ambient, "same-name", "#{pane_id}");

    let root = scratch("cyclops-ops-focus-route-config");
    let config_path = root.join("tmux.conf");
    std::fs::write(&config_path, "set-option -g status-position top\n")
        .expect("write configured tmux file");
    let cfg = ControlConfig::new_session("same-name")
        .on_socket(configured.sock().to_string())
        .with_config_file(&config_path);
    let (client, _n) = ControlClient::spawn(cfg)
        .await
        .expect("spawn configured client");
    assert_eq!(
        lines(&configured, &["show-options", "-gv", "status-position"]),
        vec!["top"],
        "the explicit config reached the same server as focus"
    );

    configured.tmux_ok(&["new-window", "-d", "-t", "same-name:", "-n", "second"]);
    let second = lines(
        &configured,
        &["list-windows", "-t", "same-name", "-F", "#{window_id}"],
    )[1]
    .clone();
    configured.tmux_ok(&["split-window", "-h", "-t", &second]);
    let second_pane = pane_ids(&configured, &second)[0].clone();

    client
        .focus_window_pane(&second, &second_pane)
        .await
        .expect("focus exact window route");
    assert_eq!(field(&configured, "same-name", "#{window_id}"), second);
    assert_eq!(field(&configured, "same-name", "#{pane_id}"), second_pane);
    assert_eq!(
        field(&ambient, "same-name", "#{pane_id}"),
        ambient_before,
        "the same-shaped ambient server was untouched"
    );

    configured.new_session("background");
    let background_id = field(&configured, "background", "#{session_id}");
    let background_window = field(&configured, "background", "#{window_id}");
    let background_pane = field(&configured, "background", "#{pane_id}");
    configured.tmux_ok(&["rename-session", "-t", &background_id, "renamed-background"]);
    configured.new_session("background");
    client
        .focus_session_window_pane(&background_id, &background_window, &background_pane)
        .await
        .expect("focus stable session route after name reuse");
    assert_eq!(
        lines(&configured, &["list-clients", "-F", "#{client_session}"]),
        vec!["renamed-background"]
    );
    assert_eq!(
        field(&configured, "renamed-background", "#{pane_id}"),
        background_pane
    );

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn horizontal_split_puts_the_new_pane_beside_the_source() {
    let Some(srv) = TestServer::new("ops-split-h") else {
        return;
    };
    srv.new_session("s");
    let source = pane_ids(&srv, "s")[0].clone();
    let session_id = field(&srv, "s", "#{session_id}");
    let window_id = field(&srv, &source, "#{window_id}");
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client
        .split_window_at(&session_id, &window_id, &source, SplitDirection::Horizontal)
        .await
        .expect("split");

    let panes = pane_ids(&srv, "s");
    assert_eq!(panes.len(), 2, "split should add exactly one pane");
    let new = panes.iter().find(|p| **p != source).expect("new pane");
    let (src_left, src_top, _) = geometry(&srv, &source);
    let (new_left, new_top, _) = geometry(&srv, new);
    assert_eq!(new_top, src_top, "a -h split shares rows with its source");
    assert!(
        new_left > src_left,
        "a -h split sits to the right of its source: {new_left} vs {src_left}"
    );
    assert_eq!(
        &field(&srv, "s", "#{pane_id}"),
        new,
        "a split exists to be typed into, so tmux focus follows it"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn vertical_split_puts_the_new_pane_below_the_source() {
    let Some(srv) = TestServer::new("ops-split-v") else {
        return;
    };
    srv.new_session("s");
    let source = pane_ids(&srv, "s")[0].clone();
    let session_id = field(&srv, "s", "#{session_id}");
    let window_id = field(&srv, &source, "#{window_id}");
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client
        .split_window_at(&session_id, &window_id, &source, SplitDirection::Vertical)
        .await
        .expect("split");

    let panes = pane_ids(&srv, "s");
    assert_eq!(panes.len(), 2);
    let new = panes.iter().find(|p| **p != source).expect("new pane");
    let (src_left, src_top, _) = geometry(&srv, &source);
    let (new_left, new_top, _) = geometry(&srv, new);
    assert_eq!(
        new_left, src_left,
        "a -v split shares columns with its source"
    );
    assert!(
        new_top > src_top,
        "a -v split sits below its source: {new_top} vs {src_top}"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn split_opens_in_the_source_panes_directory_not_the_sessions() {
    let Some(srv) = TestServer::new("ops-split-cwd") else {
        return;
    };
    let session_dir = scratch("cyclops-ops-session-dir");
    let pane_dir = scratch("cyclops-ops-pane-dir");
    // Named window: an unnamed /bin/sh window auto-renames to "sh", which bare `-t s` prefix-matches.
    srv.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-n",
        "root",
        "-x",
        "120",
        "-y",
        "30",
        "-c",
        session_dir.to_str().expect("UTF-8 scratch path"),
        "/bin/sh",
    ]);
    // A window rooted somewhere else: its pane's path is the only thing
    // that distinguishes `-c #{pane_current_path}` from tmux's default,
    // which is the session's directory.
    // Named window: an unnamed /bin/sh window auto-renames to `sh`, which bare
    // `-t s` can prefix-match. Use the explicit index too: tmux 3.7b can race
    // when choosing the next free index under parallel load.
    srv.tmux_ok(&[
        "new-window",
        "-t",
        "s:1",
        "-n",
        "work",
        "-c",
        pane_dir.to_str().expect("UTF-8 scratch path"),
        "/bin/sh",
    ]);
    // `s:1`, not the bare session `s`, below too: a session-level target
    // resolves to its *current* window, and not giving `new-window` `-d`
    // is supposed to make the window it just created current — but that
    // hand-off MEASURED racy on the same tmux 3.7b under the same load:
    // `display-message -p -t s "#{window_index}"` read back `0` right
    // after a successful, `-d`-less `new-window -t s:1` 3/80 tries. Asking
    // for window 1's panes directly does not depend on that hand-off
    // having landed yet, and reproduced zero mistargeted-pane failures
    // over 100 tries.
    let source = pane_ids(&srv, "s:1")[0].clone();
    let session_id = field(&srv, "s", "#{session_id}");
    let window_id = field(&srv, &source, "#{window_id}");
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");
    assert!(
        same_dir(&field(&srv, &source, "#{pane_current_path}"), &pane_dir),
        "fixture: the source pane should start in the pane directory"
    );

    client
        .split_window_at(&session_id, &window_id, &source, SplitDirection::Horizontal)
        .await
        .expect("split");

    let panes = pane_ids(&srv, "s:1");
    let new = panes.iter().find(|p| **p != source).expect("new pane");
    let path = field(&srv, new, "#{pane_current_path}");
    assert!(
        same_dir(&path, &pane_dir),
        "split should inherit the source pane's path, got {path} (session dir is {})",
        session_dir.display()
    );

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&session_dir);
    let _ = std::fs::remove_dir_all(&pane_dir);
}

#[tokio::test]
async fn exact_split_route_refuses_a_source_moved_to_another_workspace() {
    let Some(srv) = TestServer::new("ops-split-moved-source") else {
        return;
    };
    srv.new_session("shown");
    srv.tmux_ok(&["split-window", "-h", "-t", "shown"]);
    srv.new_session("other");
    let source = pane_ids(&srv, "shown")[0].clone();
    let session_id = field(&srv, "shown", "#{session_id}");
    let window_id = field(&srv, &source, "#{window_id}");
    let (client, _n) = ControlClient::spawn(srv.config("shown"))
        .await
        .expect("spawn");

    srv.tmux_ok(&["move-pane", "-s", &source, "-t", "other"]);
    let before_shown = pane_ids(&srv, "shown");
    let before_other = pane_ids(&srv, "other");
    let error = client
        .split_window_at(&session_id, &window_id, &source, SplitDirection::Horizontal)
        .await
        .expect_err("the old compound route must be stale");

    assert!(matches!(error, TmuxError::Command(_)), "{error}");
    assert_eq!(pane_ids(&srv, "shown"), before_shown);
    assert_eq!(
        pane_ids(&srv, "other"),
        before_other,
        "a stale route split the pane in its new workspace"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn kill_pane_closes_only_the_named_pane() {
    let Some(srv) = TestServer::new("ops-kill-pane") else {
        return;
    };
    srv.new_session("s");
    srv.tmux_ok(&["split-window", "-h", "-t", "s"]);
    let panes = pane_ids(&srv, "s");
    let (doomed, survivor) = (panes[0].clone(), panes[1].clone());
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client.kill_pane(&doomed).await.expect("kill");

    assert_eq!(pane_ids(&srv, "s"), vec![survivor]);
    client.shutdown().await;
}

#[tokio::test]
async fn resize_pane_moves_the_edge_by_the_requested_cells() {
    let Some(srv) = TestServer::new("ops-resize") else {
        return;
    };
    srv.new_session("s");
    srv.tmux_ok(&["split-window", "-h", "-t", "s"]);
    let left = pane_ids(&srv, "s")[0].clone();
    let (_, _, before) = geometry(&srv, &left);
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client
        .resize_pane(&left, PaneDirection::Right, 5)
        .await
        .expect("grow");
    let (_, _, grown) = geometry(&srv, &left);
    assert_eq!(grown, before + 5, "-R pushes the right edge outward");

    client
        .resize_pane(&left, PaneDirection::Left, 5)
        .await
        .expect("shrink");
    let (_, _, back) = geometry(&srv, &left);
    assert_eq!(back, before, "-L pulls the same edge back");

    client.shutdown().await;
}

#[tokio::test]
async fn zoom_toggles_the_window_flag_both_ways() {
    let Some(srv) = TestServer::new("ops-zoom") else {
        return;
    };
    srv.new_session("s");
    srv.tmux_ok(&["split-window", "-h", "-t", "s"]);
    let pane = pane_ids(&srv, "s")[0].clone();
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client.toggle_pane_zoom(&pane).await.expect("zoom in");
    assert_eq!(field(&srv, "s", "#{window_zoomed_flag}"), "1");
    client.toggle_pane_zoom(&pane).await.expect("zoom out");
    assert_eq!(field(&srv, "s", "#{window_zoomed_flag}"), "0");

    client.shutdown().await;
}

#[tokio::test]
async fn new_window_returns_its_id_and_applies_name_and_directory() {
    let Some(srv) = TestServer::new("ops-new-window") else {
        return;
    };
    srv.new_session("s");
    let dir = scratch("cyclops-ops-new-window");
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    // A space in the name would tokenize into two arguments unquoted.
    let id = client
        .new_window(Some("review notes"), Some(dir.as_path()))
        .await
        .expect("new window");

    assert!(id.starts_with('@'), "expected a window id, got {id:?}");
    assert_eq!(field(&srv, &id, "#{window_name}"), "review notes");
    let path = field_when_set(&srv, &id, "#{pane_current_path}");
    assert!(
        same_dir(&path, &dir),
        "new window should start in the requested directory, got {path}"
    );
    assert_eq!(window_names(&srv, "s").len(), 2);

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn new_window_without_a_name_or_directory_still_returns_an_id() {
    let Some(srv) = TestServer::new("ops-new-window-bare") else {
        return;
    };
    srv.new_session("s");
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    // Empty strings are the shape a UI's blank name field produces; they
    // must read as "not given", not as a name of "" or a missing directory.
    let id = client
        .new_window(Some(""), Some(Path::new("")))
        .await
        .expect("new window");

    assert!(id.starts_with('@'), "expected a window id, got {id:?}");
    assert_eq!(window_names(&srv, "s").len(), 2);
    client.shutdown().await;
}

#[tokio::test]
async fn rename_window_targets_the_id_not_the_current_window() {
    let Some(srv) = TestServer::new("ops-rename-window") else {
        return;
    };
    srv.new_session("s");
    // The second window is current, so an untargeted rename would hit it.
    // `s:` is the session-typed target; bare `-t s` prefix-matches window 0's auto-renamed "sh" name.
    srv.tmux_ok(&["new-window", "-t", "s:", "-n", "active", "/bin/sh"]);
    let first = lines(&srv, &["list-windows", "-t", "s", "-F", "#{window_id}"])[0].clone();
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client
        .rename_window(&first, "review notes")
        .await
        .expect("rename");

    assert_eq!(window_names(&srv, "s"), vec!["review notes", "active"]);
    client.shutdown().await;
}

#[tokio::test]
async fn rename_session_renames_exactly_that_session() {
    let Some(srv) = TestServer::new("ops-rename-session") else {
        return;
    };
    srv.new_session("host");
    srv.new_session("proj");
    srv.new_session("project");
    let (client, _n) = ControlClient::spawn(srv.config("host"))
        .await
        .expect("spawn");

    client
        .rename_session("proj", "renamed")
        .await
        .expect("rename");

    let sessions = lines(&srv, &["list-sessions", "-F", "#{session_name}"]);
    assert!(sessions.iter().any(|s| s == "renamed"));
    assert!(
        sessions.iter().any(|s| s == "project"),
        "the prefix neighbour must be untouched: {sessions:?}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn renaming_a_vanished_session_fails_instead_of_hitting_a_prefix_neighbour() {
    let Some(srv) = TestServer::new("ops-rename-exact") else {
        return;
    };
    srv.new_session("host");
    srv.new_session("proj");
    srv.new_session("project");
    let (client, _n) = ControlClient::spawn(srv.config("host"))
        .await
        .expect("spawn");
    srv.tmux_ok(&["kill-session", "-t", "=proj"]);

    let err = client.rename_session("proj", "renamed").await;

    assert!(
        matches!(err, Err(TmuxError::Command(_))),
        "a vanished exact target must fail, got {err:?}"
    );
    let sessions = lines(&srv, &["list-sessions", "-F", "#{session_name}"]);
    assert!(
        sessions.iter().any(|s| s == "project"),
        "`project` must not have been renamed: {sessions:?}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn swap_window_exchanges_the_two_positions() {
    let Some(srv) = TestServer::new("ops-swap") else {
        return;
    };
    srv.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-n",
        "one",
        "-x",
        "120",
        "-y",
        "30",
        "/bin/sh",
    ]);
    srv.tmux_ok(&["new-window", "-d", "-t", "s", "-n", "two", "/bin/sh"]);
    let ids = lines(&srv, &["list-windows", "-t", "s", "-F", "#{window_id}"]);
    let (first, second) = (ids[0].clone(), ids[1].clone());
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client.swap_window(&first, &second).await.expect("swap");

    assert_eq!(window_names(&srv, "s"), vec!["two", "one"]);
    client.shutdown().await;
}

#[tokio::test]
async fn swap_pane_exchanges_the_panes_and_focuses_the_target() {
    let Some(srv) = TestServer::new("ops-swap-pane") else {
        return;
    };
    srv.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-n",
        "one",
        "-x",
        "120",
        "-y",
        "30",
        "/bin/sh",
    ]);
    srv.tmux_ok(&["split-window", "-d", "-h", "-t", "s:one", "/bin/sh"]);
    let before = lines(
        &srv,
        &["list-panes", "-t", "s:one", "-F", "#{pane_id} #{pane_left}"],
    );
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    let (a, b) = (
        before[0].split(' ').next().expect("id").to_string(),
        before[1].split(' ').next().expect("id").to_string(),
    );
    client.swap_pane(&a, &b).await.expect("swap");

    let after = lines(
        &srv,
        &[
            "list-panes",
            "-t",
            "s:one",
            "-F",
            "#{pane_id} #{pane_left} #{pane_active}",
        ],
    );
    // Positions exchanged, and tmux focused `-t` in its new slot.
    let a_left_before = before[0].split(' ').nth(1).expect("left");
    let b_left_before = before[1].split(' ').nth(1).expect("left");
    assert!(
        after.contains(&format!("{a} {b_left_before} 0")),
        "{after:?}"
    );
    assert!(
        after.contains(&format!("{b} {a_left_before} 1")),
        "{after:?}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn swap_pane_toward_swaps_with_the_tmux_resolved_neighbour() {
    let Some(srv) = TestServer::new("ops-swap-toward") else {
        return;
    };
    srv.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-n",
        "one",
        "-x",
        "120",
        "-y",
        "30",
        "/bin/sh",
    ]);
    srv.tmux_ok(&["split-window", "-d", "-h", "-t", "s:one", "/bin/sh"]);
    let before = lines(
        &srv,
        &["list-panes", "-t", "s:one", "-F", "#{pane_id} #{pane_left}"],
    );
    let (left, right) = (
        before[0].split(' ').next().expect("id").to_string(),
        before[1].split(' ').next().expect("id").to_string(),
    );
    srv.tmux_ok(&["select-pane", "-t", &left]);
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client
        .swap_pane_toward(PaneDirection::Right)
        .await
        .expect("swap toward");

    // The current pane moved right and kept focus (it rides the implied
    // `-t`); the resolved neighbour took its old slot.
    let after = lines(
        &srv,
        &[
            "list-panes",
            "-t",
            "s:one",
            "-F",
            "#{pane_id} #{pane_left} #{pane_active}",
        ],
    );
    let left_pos = before[0].split(' ').nth(1).expect("left");
    let right_pos = before[1].split(' ').nth(1).expect("left");
    assert!(
        after.contains(&format!("{left} {right_pos} 1")),
        "{after:?}"
    );
    assert!(
        after.contains(&format!("{right} {left_pos} 0")),
        "{after:?}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn move_window_to_session_appends_it_to_the_other_session() {
    let Some(srv) = TestServer::new("ops-move") else {
        return;
    };
    srv.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "alpha",
        "-n",
        "home",
        "-x",
        "120",
        "-y",
        "30",
        "/bin/sh",
    ]);
    srv.tmux_ok(&[
        "new-window",
        "-d",
        "-t",
        "alpha",
        "-n",
        "movable",
        "/bin/sh",
    ]);
    srv.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "beta",
        "-n",
        "kept",
        "-x",
        "120",
        "-y",
        "30",
        "/bin/sh",
    ]);
    let movable = lines(&srv, &["list-windows", "-t", "alpha", "-F", "#{window_id}"])[1].clone();
    let (client, _n) = ControlClient::spawn(srv.config("alpha"))
        .await
        .expect("spawn");

    client
        .move_window_to_session(&movable, "beta")
        .await
        .expect("move");

    assert_eq!(window_names(&srv, "alpha"), vec!["home"]);
    assert_eq!(
        window_names(&srv, "beta"),
        vec!["kept", "movable"],
        "the trailing colon appends rather than colliding with an index"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn select_window_makes_it_current() {
    let Some(srv) = TestServer::new("ops-select-window") else {
        return;
    };
    srv.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-n",
        "one",
        "-x",
        "120",
        "-y",
        "30",
        "/bin/sh",
    ]);
    srv.tmux_ok(&["new-window", "-d", "-t", "s", "-n", "two", "/bin/sh"]);
    let second = lines(&srv, &["list-windows", "-t", "s", "-F", "#{window_id}"])[1].clone();
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client.select_window(&second).await.expect("select");

    assert_eq!(field(&srv, "s", "#{window_name}"), "two");
    client.shutdown().await;
}

#[tokio::test]
async fn switch_to_session_moves_this_client() {
    let Some(srv) = TestServer::new("ops-switch") else {
        return;
    };
    srv.new_session("alpha");
    srv.new_session("beta");
    let (client, _n) = ControlClient::spawn(srv.config("alpha"))
        .await
        .expect("spawn");

    client.switch_to_session("beta").await.expect("switch");

    let attached = lines(&srv, &["list-clients", "-F", "#{client_session}"]);
    assert_eq!(attached, vec!["beta"], "the control client must move");
    client.shutdown().await;
}

#[tokio::test]
async fn new_session_returns_its_id_detached_in_the_requested_directory() {
    let Some(srv) = TestServer::new("ops-new-session") else {
        return;
    };
    srv.new_session("host");
    let dir = scratch("cyclops-ops-new-session");
    let (client, _n) = ControlClient::spawn(srv.config("host"))
        .await
        .expect("spawn");

    let id = client
        .new_session("proj", dir.as_path())
        .await
        .expect("new session");

    assert!(id.starts_with('$'), "expected a session id, got {id:?}");
    let sessions = lines(&srv, &["list-sessions", "-F", "#{session_name}"]);
    assert!(sessions.iter().any(|s| s == "proj"), "{sessions:?}");
    // Detached creation: this client must still be looking at `host`.
    let attached = lines(&srv, &["list-clients", "-F", "#{client_session}"]);
    assert_eq!(attached, vec!["host"], "-d must not steal the client");
    let path = field_when_set(&srv, &format!("{id}:"), "#{pane_current_path}");
    assert!(
        same_dir(&path, &dir),
        "the session should start in the requested directory, got {path}"
    );
    assert_eq!(window_names(&srv, "proj"), vec!["1"]);

    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn kill_session_closes_exactly_that_session() {
    let Some(srv) = TestServer::new("ops-kill-session") else {
        return;
    };
    srv.new_session("host");
    srv.new_session("proj");
    srv.new_session("project");
    let (client, _n) = ControlClient::spawn(srv.config("host"))
        .await
        .expect("spawn");

    client.kill_session("proj").await.expect("kill");

    let sessions = lines(&srv, &["list-sessions", "-F", "#{session_name}"]);
    assert!(!sessions.iter().any(|s| s == "proj"), "{sessions:?}");
    assert!(
        sessions.iter().any(|s| s == "project"),
        "the prefix neighbour must survive an exact-match kill: {sessions:?}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn kill_window_closes_the_named_window() {
    let Some(srv) = TestServer::new("ops-kill-window") else {
        return;
    };
    srv.tmux_ok(&[
        "new-session",
        "-d",
        "-s",
        "s",
        "-n",
        "keep",
        "-x",
        "120",
        "-y",
        "30",
        "/bin/sh",
    ]);
    srv.tmux_ok(&["new-window", "-d", "-t", "s", "-n", "doomed", "/bin/sh"]);
    let doomed = lines(&srv, &["list-windows", "-t", "s", "-F", "#{window_id}"])[1].clone();
    let (client, _n) = ControlClient::spawn(srv.config("s")).await.expect("spawn");

    client.kill_window(&doomed).await.expect("kill");

    assert_eq!(window_names(&srv, "s"), vec!["keep"]);
    client.shutdown().await;
}
