//! Layouts against a real tmux server: build one, read it back, build the
//! reading again.
//!
//! Every test runs on its own `-L cyc-<tag>-<pid>-<sequence>` server with
//! `-f /dev/null`, from `cyclops-testrig`, which also kills it and unlinks
//! the socket on drop. Nothing here can touch the developer's tmux.

use cyclops_testrig::{tmux_available, TmuxServer};
use cyclops_tmux::layout::{self, ApplyOptions, Capture, Layout, Server};
use cyclops_tmux::TmuxError;

/// The size every test builds at, so the cell arithmetic is the same on
/// every machine. Real runs take the size of the terminal instead.
const SIZE: Option<(u32, u32)> = Some((200, 50));

/// How far a captured ratio may sit from the designed one. Cells are whole
/// numbers: at 49 usable rows, 0.70 can only be 34/49 or 35/49, which is
/// 0.694 or 0.714.
const SLACK: f64 = 0.02;

fn ops() -> Layout {
    let text = include_str!("../../../resources/layouts/ops.toml");
    toml::from_str(text).expect("the shipped ops preset parses")
}

fn server(t: &TmuxServer) -> Server {
    Server {
        socket: Some(t.socket().to_string()),
        config_file: Some("/dev/null".into()),
    }
}

fn build(t: &TmuxServer, session: &str, layout: &Layout) -> Vec<String> {
    layout::apply(
        &server(t),
        session,
        layout,
        &ApplyOptions {
            launch: false,
            size: SIZE,
        },
    )
    .expect("layout applies")
}

fn read(t: &TmuxServer, session: &str) -> Capture {
    layout::capture(&server(t), session).expect("layout captures")
}

fn close(got: f64, want: f64, what: &str) {
    assert!(
        (got - want).abs() <= SLACK,
        "{what}: got {got}, wanted {want} give or take {SLACK}"
    );
}

#[test]
fn the_ops_dock_lands_at_the_ratio_it_was_designed_with() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("layout-ops");
    let ids = build(&t, "ops", &ops());
    assert_eq!(ids.len(), 4, "three agents and the dock");

    let cap = read(&t, "ops");
    let rows = &cap.layout.windows[0].rows;
    assert_eq!(rows.len(), 2, "agents on top, dock underneath");
    close(rows[0].ratio, 0.70, "the agent row");
    close(rows[1].ratio, 0.30, "the dock");
    assert_eq!(rows[0].panes.len(), 3);
    assert_eq!(rows[1].panes.len(), 1, "the dock is one full-width pane");
    for p in &rows[0].panes {
        close(p.ratio, 1.0 / 3.0, "an agent pane");
    }
    // Position order, not tmux's pane index order: tmux lists these as
    // %0 %2 %3 %1, and a caller zipping labels onto them by index would
    // name the dock "reviewer".
    assert_eq!(cap.pane_ids, ids);
}

/// Regression, and it came from running the demo rather than from reading
/// the code: cyclops's own pane chrome turns on `pane-border-status top`,
/// which costs every pane a line (F27). The grid used to be checked
/// against the WINDOW's size, so the moment the daemon named a pane the
/// numbers stopped adding up and `cyclops workspace save` refused a
/// session `cyclops start` had built seconds earlier. Measuring the panes
/// instead is what makes it hold either way.
#[test]
fn a_window_wearing_border_chrome_still_reads_as_a_grid() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("layout-chrome");
    build(&t, "chrome", &ops());
    let bare = read(&t, "chrome");
    t.run_ok(&[
        "set-window-option",
        "-t",
        "=chrome",
        "pane-border-status",
        "top",
    ]);

    let worn = read(&t, "chrome");
    let rows = &worn.layout.windows[0].rows;
    assert_eq!(rows.len(), 2, "still agents over a dock");
    assert_eq!(rows[0].panes.len(), 3);
    assert_eq!(worn.pane_ids, bare.pane_ids);
    // The borders take their lines out of the panes, so the shares move a
    // little. The design is still recognisable, which is the claim.
    assert!(
        (rows[1].ratio - 0.30).abs() <= 0.05,
        "the dock reads as {} with chrome on",
        rows[1].ratio
    );
}

#[test]
fn a_captured_layout_builds_back_into_the_same_shape() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("layout-trip");
    build(&t, "first", &ops());
    let first = read(&t, "first");

    // The captured layout is the input this time: same ratios, same
    // window name, and the directories tmux reported.
    build(&t, "second", &first.layout);
    let second = read(&t, "second");

    assert_eq!(second.layout.windows.len(), first.layout.windows.len());
    for (a, b) in first
        .layout
        .windows
        .iter()
        .zip(second.layout.windows.iter())
    {
        assert_eq!(a.name, b.name, "window name survives");
        assert_eq!(a.rows.len(), b.rows.len());
        for (ra, rb) in a.rows.iter().zip(b.rows.iter()) {
            close(rb.ratio, ra.ratio, "a row");
            assert_eq!(ra.panes.len(), rb.panes.len());
            for (pa, pb) in ra.panes.iter().zip(rb.panes.iter()) {
                close(pb.ratio, pa.ratio, "a pane");
                assert_eq!(pa.cwd, pb.cwd, "the working directory survives");
            }
        }
    }
}

#[test]
fn a_recorded_command_runs_only_when_it_is_asked_for() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("layout-launch");
    // `cat` sits there holding the pane, so pane_current_command is a
    // stable thing to read back.
    let mut layout = ops();
    layout.windows[0].rows[1].panes[0].command = Some("cat".to_string());

    build(&t, "quiet", &layout);
    let dock = &read(&t, "quiet").layout.windows[0].rows[1].panes[0];
    assert_eq!(
        dock.command, None,
        "structure, not processes: the pane holds a shell"
    );

    layout::apply(
        &server(&t),
        "loud",
        &layout,
        &ApplyOptions {
            launch: true,
            size: SIZE,
        },
    )
    .expect("layout applies");
    // tmux spawns the command asynchronously, so give the pane a moment to
    // become it. Test-side wait; the product never polls for this.
    let mut command = None;
    for _ in 0..50 {
        command = read(&t, "loud").layout.windows[0].rows[1].panes[0]
            .command
            .clone();
        if command.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(command.as_deref(), Some("cat"), "--launch ran the command");
}

#[test]
fn a_session_that_already_exists_is_never_rearranged() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("layout-exists");
    build(&t, "taken", &ops());
    let before = read(&t, "taken");

    let err = layout::apply(
        &server(&t),
        "taken",
        &ops(),
        &ApplyOptions {
            launch: false,
            size: SIZE,
        },
    )
    .expect_err("a second apply is refused");
    match &err {
        TmuxError::Layout(msg) => assert!(msg.contains("already exists"), "{msg}"),
        other => panic!("wrong error: {other:?}"),
    }
    assert_eq!(read(&t, "taken").pane_ids, before.pane_ids, "untouched");
}

#[test]
fn a_nested_split_is_refused_instead_of_saved_wrong() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("layout-nested");
    t.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "nested",
        "-x",
        "200",
        "-y",
        "50",
        "/bin/sh",
    ]);
    // A pane beside another, then a pane under only one of them: the
    // second row does not span the window, so it is not a row.
    t.run_ok(&["split-window", "-h", "-d", "-t", "nested:0.0", "/bin/sh"]);
    t.run_ok(&["split-window", "-v", "-d", "-t", "nested:0.0", "/bin/sh"]);

    let err = layout::capture(&server(&t), "nested").expect_err("not a grid");
    match &err {
        TmuxError::Layout(msg) => {
            assert!(msg.contains("not a grid of rows"), "{msg}");
            assert!(
                msg.contains("select-layout"),
                "the next step is named: {msg}"
            );
        }
        other => panic!("wrong error: {other:?}"),
    }
}

#[test]
fn a_zoomed_pane_is_named_rather_than_guessed_at() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("layout-zoom");
    build(&t, "zoom", &ops());
    let ids = read(&t, "zoom").pane_ids;
    t.run_ok(&["resize-pane", "-Z", "-t", &ids[0]]);

    let err = layout::capture(&server(&t), "zoom").expect_err("zoom is refused");
    match &err {
        TmuxError::Layout(msg) => assert!(msg.contains("zoomed"), "{msg}"),
        other => panic!("wrong error: {other:?}"),
    }
}

#[test]
fn asking_about_a_session_that_is_not_there_is_not_an_error() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("layout-exist");
    // No server is running yet on this socket: still an answer, not a
    // failure, because that is the state `cyclops start` starts from.
    assert!(!layout::session_exists(&server(&t), "ghost").expect("answers"));
    build(&t, "real", &ops());
    assert!(layout::session_exists(&server(&t), "real").expect("answers"));
    // Exact match: "real" must not answer for "rea".
    assert!(!layout::session_exists(&server(&t), "rea").expect("answers"));
}
