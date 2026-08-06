//! `cyclops start` and `cyclops workspace` end to end: the real binary, an
//! isolated tmux server, and a canned daemon on a scratch socket.
//!
//! The tmux server is `cyclops-testrig`'s, so it is a unique `-L` name with
//! `-f /dev/null` and it is killed and unlinked on drop. The daemon is
//! canned rather than real because what these verbs need from it is one
//! answer and a handful of labels, and the CLI crate is where the copy
//! lives. No network, no default tmux server, no real home.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use cyclops_testrig::{tmux_available, TmuxServer};
use serde_json::{json, Value};

/// Scratch CYCLOPS_HOME under the relocatable scratch root (F24).
fn scratch_home(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch home");
    dir
}

fn write_config(home: &Path, t: &TmuxServer, body: &str) {
    fs::write(
        home.join("config.toml"),
        format!(
            "tmux_socket = \"{}\"\ntmux_config = \"/dev/null\"\n{body}",
            t.socket()
        ),
    )
    .expect("write config");
}

fn cyclops(home: &Path, args: &[&str]) -> Output {
    cyclops_raw(home, args, true)
}

/// `cyclops` with `start` allowed to launch a daemon, for the one test
/// that is about that.
fn cyclops_with_daemon(home: &Path, args: &[&str]) -> Output {
    cyclops_raw(home, args, false)
}

/// `no_daemon` adds `--no-daemon` to a `start`, because these tests are
/// about what `start` does to tmux and to the workspace file, and a real
/// daemon per test would be a process to reap, a socket to collide on,
/// and a source of timing in assertions that have none today. The spawn
/// path is covered where it belongs: on a real rig, in
/// tests/e2e/parity-check.sh.
fn cyclops_raw(home: &Path, args: &[&str], no_daemon: bool) -> Output {
    let mut argv: Vec<&str> = args.to_vec();
    if no_daemon && args.first() == Some(&"start") && !args.contains(&"--setup-only") {
        argv.push("--no-daemon");
    }
    Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("CYCLOPS_THEME")
        // `start` offers `tmux attach` only outside tmux, so an inherited
        // TMUX would give a developer running the suite from inside tmux
        // different output than CI. The rule gets its own test below.
        .env_remove("TMUX")
        .args(&argv)
        .output()
        .expect("run cyclops")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Panes of a session in position order, as tmux reports them.
fn panes(t: &TmuxServer, session: &str) -> Vec<(String, u32, u32)> {
    let out = t.run(&[
        "list-panes",
        "-s",
        "-t",
        &format!("={session}"),
        "-F",
        "#{pane_id} #{pane_left} #{pane_top}",
    ]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut rows: Vec<(String, u32, u32)> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (
                f[0].to_string(),
                f[1].parse().unwrap(),
                f[2].parse().unwrap(),
            )
        })
        .collect();
    rows.sort_by_key(|(_, left, top)| (*top, *left));
    rows
}

/// A daemon that answers `status` with one watched session and accepts
/// every `pane.label`, recording what it was asked. `conns` sequential
/// connections, one per cyclops run. Each pane is an id and the name the
/// registry has for it, which is where `save` gets the names it writes.
/// `taken` is names held in some OTHER watched session, which the real
/// daemon refuses for the same reason it refuses one held here.
fn canned_daemon(
    home: &Path,
    conns: usize,
    session: &str,
    panes: Vec<(String, Option<String>)>,
    taken: &[&str],
) -> Arc<Mutex<Vec<Value>>> {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(home.join("sock")).expect("bind scratch socket");
    let record = Arc::clone(&seen);
    let session = session.to_string();
    let taken: Vec<String> = taken.iter().map(|s| s.to_string()).collect();
    thread::spawn(move || {
        for _ in 0..conns {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut w = stream;
            let hello = json!({"cyclops": "0.1.0", "proto": 1, "boot_id": "b-ws"});
            if writeln!(w, "{hello}").is_err() {
                return;
            }
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let req: Value = serde_json::from_str(line.trim()).expect("request parses");
                record.lock().expect("record").push(req.clone());
                // A name is an address and is unique across watched
                // sessions, so the real daemon refuses one another pane
                // already holds. Mirrored here because what the CLI does
                // with a refusal is the thing under test.
                if req["method"] == json!("pane.label") {
                    let target = req["params"]["target"].as_str().unwrap_or_default();
                    let label = req["params"]["label"].as_str().unwrap_or_default();
                    let held_here = panes
                        .iter()
                        .any(|(id, held)| held.as_deref() == Some(label) && id != target);
                    if held_here || taken.iter().any(|t| t == label) {
                        let err = json!({
                            "id": req["id"],
                            "error": {"code": "bad_request", "message": format!("label {label:?} is already taken")},
                        });
                        if writeln!(w, "{err}").is_err() {
                            break;
                        }
                        continue;
                    }
                }
                let result = match req["method"].as_str() {
                    Some("status") => json!({
                        "daemon_version": "0.1.0",
                        "proto": 1,
                        "boot_id": "b-ws",
                        "uptime_ms": 1,
                        "tmux_version": "tmux 3.6a",
                        "sessions": [{
                            "name": session,
                            "attached": true,
                            "panes": panes.iter().map(|(id, agent)| match agent {
                                Some(a) => json!({"pane_id": id, "agent": a}),
                                None => json!({"pane_id": id}),
                            }).collect::<Vec<_>>(),
                        }],
                    }),
                    _ => json!({"ok": true}),
                };
                if writeln!(w, "{}", json!({"id": req["id"], "result": result})).is_err() {
                    break;
                }
            }
        }
    });
    seen
}

/// Pane ids the daemon knows but has no name for.
fn unnamed(ids: &[String]) -> Vec<(String, Option<String>)> {
    ids.iter().map(|id| (id.clone(), None)).collect()
}

fn label_calls(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<(String, String)> {
    seen.lock()
        .expect("record")
        .iter()
        .filter(|r| r["method"] == json!("pane.label"))
        .map(|r| {
            (
                r["params"]["target"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                r["params"]["label"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

#[test]
fn start_builds_the_workspace_and_says_what_is_left_to_do() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-start");
    let home = scratch_home("ws-start");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );

    let out = cyclops(&home, &["start", "--preset", "duo"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(
        text.starts_with("✓ workspace ready · 2 agents\n"),
        "got {text:?}"
    );
    // --no-daemon, so nothing is watching and nothing got named. The
    // steps say so: `cyclops start` puts the names on (and starts a
    // daemon, which is why there is no separate step for that), then
    // open the workspace.
    assert!(text.contains("Next:"), "{text}");
    assert!(text.contains("cyclops start"), "{text}");
    assert!(text.contains("tmux attach -t duo"), "{text}");
    assert!(
        !text.contains("cyclopsd &"),
        "the daemon is not started by hand any more: {text}"
    );
    // And no send step. Only cyclopsd holds a name, so with it down
    // nothing is named and `cyclops send implementer` would answer "no
    // pane for implementer": a printed step that cannot work.
    assert!(!text.contains("cyclops send"), "{text}");
    assert!(text.contains("nothing was named yet"), "{text}");
    assert_eq!(panes(&t, "duo").len(), 2);

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn start_runs_twice_without_building_anything_twice() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-again");
    let home = scratch_home("ws-again");
    write_config(
        &home,
        &t,
        "sessions = [\"ops\"]\ndefault_workspace = \"ops\"\n",
    );

    assert!(cyclops(&home, &["start", "--preset", "ops"])
        .status
        .success());
    let first = panes(&t, "ops");
    assert_eq!(first.len(), 4);

    let out = cyclops(&home, &["start", "--preset", "ops"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.starts_with("✓ workspace ready · 3 agents\n"), "{text}");
    // Same panes, same ids: nothing was rebuilt, nothing was added.
    assert_eq!(panes(&t, "ops"), first);

    let _ = fs::remove_dir_all(&home);
}

/// `--setup-only` is the installer's last step: make the home usable, and
/// touch tmux for nothing. Needs no tmux server, which is the point.
#[test]
fn setup_only_writes_the_home_and_opens_nothing() {
    let home = scratch_home("ws-setup");

    let out = cyclops(&home, &["start", "--setup-only"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(text.starts_with("✔ cyclops is set up\n"), "got {text:?}");
    assert!(text.contains("config.toml"), "{text}");
    assert!(text.contains("4 detection manifests"), "{text}");
    // No workspace, so no next steps: whoever called this owns what comes
    // after it.
    assert!(!text.contains("Next:"), "{text}");

    assert!(home.join("config.toml").is_file());
    assert!(home.join("manifests/claude.toml").is_file());

    // Twice is a no-op. The installer runs it on every install, including
    // the ones over a home that is already set up.
    let again = stdout(&cyclops(&home, &["start", "--setup-only"]));
    assert!(again.starts_with("✔ cyclops is set up\n"), "got {again:?}");
    assert!(!again.contains("wrote"), "{again}");

    let _ = fs::remove_dir_all(&home);
}

/// The `tmux attach` step follows where you are, not what this run built.
///
/// It used to appear only when `start` created the session, which dropped
/// it from the second run of a first setup: the run where the session
/// exists, the panes still hold no agent, and opening it is the whole
/// point. Inside tmux there is nothing to attach to, so it goes away.
#[test]
fn the_attach_step_follows_whether_you_are_inside_tmux() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-inside");
    let home = scratch_home("ws-inside");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );

    // First run builds it, second finds it there. Outside tmux both offer
    // the step.
    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    let again = stdout(&cyclops(&home, &["start"]));
    assert!(again.contains("tmux attach -t duo"), "{again}");

    let inside = Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", &home)
        .env("NO_COLOR", "1")
        .env("TMUX", "/tmp/tmux-501/default,12345,0")
        .args(["start"])
        .output()
        .expect("run cyclops");
    let text = stdout(&inside);
    assert!(!text.contains("tmux attach"), "{text}");

    let _ = fs::remove_dir_all(&home);
}

/// Regression: a `--preset` build used to leave nothing behind, so the
/// next bare `start` fell back to `solo` and reported one agent over a
/// two-agent session. The count a person reads has to come from something
/// that describes the session in front of them.
#[test]
fn a_preset_build_leaves_the_workspace_behind_for_the_next_run() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-persist");
    let home = scratch_home("ws-persist");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );

    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    let saved = fs::read_to_string(home.join("workspaces/duo.toml")).expect("start saved it");
    assert!(saved.contains("name = \"duo\""), "{saved}");
    assert!(
        saved.contains("implementer") && saved.contains("reviewer"),
        "{saved}"
    );

    let out = cyclops(&home, &["start"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout(&out).starts_with("✓ workspace ready · 2 agents\n"),
        "{}",
        stdout(&out)
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_session_that_stopped_matching_the_workspace_is_never_renamed() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-moved");
    let home = scratch_home("ws-moved");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );
    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    // A person splits a pane. The workspace now describes something else,
    // so the third pane could be any of the three as far as names go.
    t.run_ok(&["split-window", "-h", "-d", "-t", "duo:0.0"]);
    let ids: Vec<String> = panes(&t, "duo").into_iter().map(|(id, _, _)| id).collect();
    let seen = canned_daemon(&home, 1, "duo", unnamed(&ids), &[]);

    let out = cyclops(&home, &["start"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(
        text.contains("has 3 panes and the workspace describes 2"),
        "{text}"
    );
    assert!(text.contains("cyclops workspace save duo"), "{text}");
    assert!(label_calls(&seen).is_empty(), "nothing was renamed");
    // Nothing is named, and the count says so rather than repeating the
    // workspace's intent as if it were fact.
    assert!(text.starts_with("✔ workspace ready · 0 agents\n"), "{text}");

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn start_puts_the_names_back_on_the_panes() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-adopt");
    let home = scratch_home("ws-adopt");
    write_config(
        &home,
        &t,
        "sessions = [\"ops\"]\ndefault_workspace = \"ops\"\n",
    );

    // Build it first with no daemon listening, so the pane ids exist
    // before the canned daemon has to name them.
    assert!(cyclops(&home, &["start", "--preset", "ops"])
        .status
        .success());
    let ids: Vec<String> = panes(&t, "ops").into_iter().map(|(id, _, _)| id).collect();
    let seen = canned_daemon(&home, 1, "ops", unnamed(&ids), &[]);

    let out = cyclops(&home, &["start", "--preset", "ops"]);
    assert!(out.status.success(), "{out:?}");
    // Position order, not tmux's index order: the dock is the last pane
    // and gets no label, and each agent gets the one above it.
    assert_eq!(
        label_calls(&seen),
        vec![
            (ids[0].clone(), "implementer".to_string()),
            (ids[1].clone(), "reviewer".to_string()),
            (ids[2].clone(), "tests".to_string()),
        ]
    );

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_config_that_does_not_watch_the_session_gets_the_line_to_add() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-cfg");
    let home = scratch_home("ws-cfg");
    write_config(&home, &t, "sessions = [\"somewhere-else\"]\n");

    let out = cyclops(&home, &["start", "--workspace", "mine"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(text.contains("won't watch \"mine\""), "{text}");
    assert!(text.contains("config.toml"), "{text}");
    // The config the user wrote is never edited underneath them.
    let cfg = fs::read_to_string(home.join("config.toml")).expect("config still there");
    assert!(cfg.contains("somewhere-else"), "{cfg}");
    assert!(!cfg.contains("mine"), "{cfg}");

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn save_then_restore_rebuilds_the_same_shape_under_a_new_session() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-trip");
    let home = scratch_home("ws-trip");
    write_config(
        &home,
        &t,
        "sessions = [\"quad\"]\ndefault_workspace = \"quad\"\n",
    );
    assert!(cyclops(&home, &["start", "--preset", "quad"])
        .status
        .success());
    let before = panes(&t, "quad");
    assert_eq!(before.len(), 4);

    let saved = cyclops(&home, &["workspace", "save"]);
    assert!(saved.status.success(), "{saved:?}");
    assert!(stdout(&saved).contains("✓ workspace saved · quad · 4 panes"));
    assert!(home.join("workspaces/quad.toml").is_file());

    let out = cyclops(
        &home,
        &["workspace", "restore", "quad", "--session", "copy"],
    );
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(
        text.starts_with("✓ workspace restored · copy · 4 panes"),
        "{text}"
    );

    // Same geometry, pane for pane. Ids differ; positions do not.
    let after = panes(&t, "copy");
    let shape = |rows: &[(String, u32, u32)]| -> Vec<(u32, u32)> {
        rows.iter().map(|(_, l, top)| (*l, *top)).collect()
    };
    assert_eq!(shape(&after), shape(&before));

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_restore_leaves_the_panes_empty_and_says_how_to_fill_them() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-launch");
    let home = scratch_home("ws-launch");
    write_config(
        &home,
        &t,
        "sessions = [\"ops\"]\ndefault_workspace = \"ops\"\n",
    );
    assert!(cyclops(&home, &["start", "--preset", "ops"])
        .status
        .success());
    // Save the ops session, then teach the file a command the way an
    // editor would, since the panes here are only shells.
    assert!(cyclops(&home, &["workspace", "save"]).status.success());
    let path = home.join("workspaces/ops.toml");
    let text = fs::read_to_string(&path).expect("saved file");
    fs::write(&path, format!("{text}command = \"cat\"\n")).expect("edit the saved file");

    let out = cyclops(
        &home,
        &["workspace", "restore", "ops", "--session", "quiet"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout(&out).contains("restores structure, not running agents"),
        "{}",
        stdout(&out)
    );

    let _ = fs::remove_dir_all(&home);
}

/// Both of these fail before they would reach tmux, and they still name an
/// isolated server in their config: a test that only reaches the default
/// tmux server when a future edit reorders two checks is a test that has
/// not been isolated, it has been lucky.
/// The whole point of the file: the roster outlives the panes. Save reads
/// the names from the daemon, writes them next to the geometry, and a
/// restore into a fresh session hands them straight back.
#[test]
fn saving_writes_the_names_down_and_restoring_hands_them_back() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-names");
    let home = scratch_home("ws-names");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );
    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    let ids: Vec<String> = panes(&t, "duo").into_iter().map(|(id, _, _)| id).collect();
    // Two runs share this daemon: the save that reads the names, and the
    // restore that puts them on the new panes.
    let seen = canned_daemon(
        &home,
        2,
        "duo",
        vec![
            (ids[0].clone(), Some("implementer".to_string())),
            (ids[1].clone(), Some("reviewer".to_string())),
        ],
        &[],
    );

    let saved = cyclops(&home, &["workspace", "save", "named"]);
    assert!(saved.status.success(), "{saved:?}");
    assert!(stdout(&saved).contains("2 agents"), "{}", stdout(&saved));
    let file = fs::read_to_string(home.join("workspaces/named.toml")).expect("saved file");
    assert!(file.contains("label = \"implementer\""), "{file}");
    assert!(file.contains("label = \"reviewer\""), "{file}");

    let out = cyclops(
        &home,
        &["workspace", "restore", "named", "--session", "again"],
    );
    assert!(out.status.success(), "{out:?}");
    // The canned daemon watches "duo" only, so the restore into "again"
    // can name nothing, says exactly why, and names the command that will
    // do it later. The names are in the file either way.
    let text = stdout(&out);
    assert!(text.contains("cyclopsd isn't watching \"again\""), "{text}");
    assert!(
        text.contains("cyclops start --workspace named --session again"),
        "{text}"
    );
    assert!(label_calls(&seen).is_empty());

    let _ = fs::remove_dir_all(&home);
}

/// The other half of that message. A restore into a session the daemon
/// has not picked up yet names nothing, and the line it prints is a
/// command: this is that command, doing what it says.
#[test]
fn start_names_a_restored_copy_when_the_daemon_catches_up() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-catchup");
    let home = scratch_home("ws-catchup");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );
    // The preset build writes the workspace file, names and all, so the
    // restore below has a roster to carry even with no daemon anywhere.
    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    let out = cyclops(&home, &["workspace", "restore", "duo", "--session", "copy"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout(&out).contains("cyclops start --workspace duo --session copy"),
        "{}",
        stdout(&out)
    );

    // The daemon shows up afterwards, watching the copy.
    let ids: Vec<String> = panes(&t, "copy").into_iter().map(|(id, _, _)| id).collect();
    let seen = canned_daemon(&home, 1, "copy", unnamed(&ids), &[]);

    let out = cyclops(&home, &["start", "--workspace", "duo", "--session", "copy"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        label_calls(&seen),
        vec![
            (ids[0].clone(), "implementer".to_string()),
            (ids[1].clone(), "reviewer".to_string()),
        ]
    );
    // Nothing was rebuilt: the session was already there.
    assert_eq!(panes(&t, "copy").len(), 2);

    let _ = fs::remove_dir_all(&home);
}

/// When the daemon refuses a name, its answer is the whole explanation.
/// `start` used to print the refusals AND a line of its own guessing that
/// the session's shape had changed, which was both wrong and the louder of
/// the two. The name here is held in another watched session, which is
/// the one refusal `start` cannot see coming: names are addresses and are
/// unique across every session the daemon watches.
#[test]
fn a_refused_name_is_reported_once_by_the_one_who_refused_it() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-taken");
    let home = scratch_home("ws-taken");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );
    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    let ids: Vec<String> = panes(&t, "duo").into_iter().map(|(id, _, _)| id).collect();
    let _seen = canned_daemon(&home, 1, "duo", unnamed(&ids), &["implementer"]);

    let out = cyclops(&home, &["start"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(text.contains("\"implementer\" is already taken"), "{text}");
    assert!(!text.contains("no longer has the shape"), "{text}");
    assert!(!text.contains("have moved since"), "{text}");
    // One pane ends up named, and the count says one.
    assert!(text.starts_with("✔ workspace ready · 1 agent\n"), "{text}");

    let _ = fs::remove_dir_all(&home);
}

/// The cardinal rule, end to end. `ops` and `quad` are both four panes,
/// so a check that counts panes calls a session rearranged from one into
/// the other a match, and renames all three agents onto panes they do not
/// own. A name is what every later delivery resolves through, so the next
/// message would go to the wrong agent (GOALS).
#[test]
fn a_rearranged_session_with_the_same_pane_count_is_never_renamed() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-tiled");
    let home = scratch_home("ws-tiled");
    write_config(
        &home,
        &t,
        "sessions = [\"ops\"]\ndefault_workspace = \"ops\"\n",
    );
    assert!(cyclops(&home, &["start", "--preset", "ops"])
        .status
        .success());
    // Three across with a dock underneath, rearranged into two by two by
    // the person whose session it is. Same four panes, same four ids.
    let before = panes(&t, "ops");
    t.run_ok(&["select-layout", "-t", "ops:0", "tiled"]);
    let ids: Vec<String> = panes(&t, "ops").into_iter().map(|(id, _, _)| id).collect();
    assert_eq!(ids.len(), before.len(), "the same panes, moved");
    let seen = canned_daemon(&home, 1, "ops", unnamed(&ids), &[]);

    let out = cyclops(&home, &["start"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(label_calls(&seen).is_empty(), "nothing was renamed: {text}");
    assert!(text.contains("no longer has the shape"), "{text}");
    assert!(text.contains("row 1"), "it says where they differ: {text}");
    assert!(
        text.contains("cyclops workspace save ops --session ops"),
        "{text}"
    );
    assert!(text.starts_with("✔ workspace ready · 0 agents\n"), "{text}");

    let _ = fs::remove_dir_all(&home);
}

/// The check the grid cannot make. A pane that already answers to a name
/// the workspace puts somewhere else means the panes moved under the
/// file, and position stops identifying anything.
///
/// This is the partial swap, which is the dangerous one: the daemon
/// refuses "implementer" for the first pane because the second holds it,
/// and then happily renames that second pane to "reviewer". The agent
/// everyone was addressing as implementer answers to reviewer from then
/// on, and nothing said so.
#[test]
fn a_pane_that_answers_to_another_name_stops_every_rename() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-swap");
    let home = scratch_home("ws-swap");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );
    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    let ids: Vec<String> = panes(&t, "duo").into_iter().map(|(id, _, _)| id).collect();
    // The workspace puts "implementer" first and "reviewer" second. The
    // roster has "implementer" on the second pane.
    let seen = canned_daemon(
        &home,
        1,
        "duo",
        vec![
            (ids[0].clone(), None),
            (ids[1].clone(), Some("implementer".to_string())),
        ],
        &[],
    );

    let out = cyclops(&home, &["start"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(label_calls(&seen).is_empty(), "nothing was renamed: {text}");
    assert!(text.contains("have moved since"), "{text}");
    assert!(
        text.contains(&format!("{} answers to \"implementer\"", ids[1])),
        "{text}"
    );
    assert!(text.contains("wrong pane sends the next message"), "{text}");
    // The one name that is on a pane is still on it, and still counted.
    assert!(text.starts_with("✔ workspace ready · 1 agent\n"), "{text}");

    let _ = fs::remove_dir_all(&home);
}

/// Adoption is explicit (docs/guides/panes.md): cyclops never names a pane
/// because it looks like an agent. A session the operator built by hand
/// and a preset nobody chose are exactly that guess, however well the
/// pane count lines up.
#[test]
fn start_never_names_panes_in_a_session_it_did_not_build() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-theirs");
    let home = scratch_home("ws-theirs");
    write_config(
        &home,
        &t,
        "sessions = [\"mine\"]\ndefault_workspace = \"mine\"\n",
    );
    // Their session, their pane. No workspace was ever saved for it, so
    // the only layout `start` has is the solo preset, which also has one
    // pane.
    t.run_ok(&["new-session", "-d", "-s", "mine"]);
    let ids: Vec<String> = panes(&t, "mine").into_iter().map(|(id, _, _)| id).collect();
    assert_eq!(ids.len(), 1);
    let seen = canned_daemon(&home, 1, "mine", unnamed(&ids), &[]);

    let out = cyclops(&home, &["start"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(label_calls(&seen).is_empty(), "nothing was named: {text}");
    assert!(
        text.contains("no workspace called \"mine\" is saved"),
        "{text}"
    );
    assert!(
        text.contains("only puts names on panes you named"),
        "{text}"
    );
    assert!(
        text.contains("cyclops workspace save mine --session mine"),
        "{text}"
    );
    assert!(text.starts_with("✔ workspace ready · 0 agents\n"), "{text}");
    // And the guided moment does not offer to message an agent that does
    // not exist: the preset's "implementer" is nobody here.
    assert!(!text.contains("cyclops send"), "{text}");
    // Nothing was written either: a preset nobody chose is not this
    // session's workspace.
    assert!(!home.join("workspaces/mine.toml").exists());

    let _ = fs::remove_dir_all(&home);
}

/// A save with no daemon must not delete the roster.
///
/// The names are in exactly two places, the registry and this file, and
/// the registry is the one that cannot be reached here. Writing the file
/// without them leaves them nowhere, and no command on the machine can
/// get them back.
#[test]
fn save_without_a_daemon_keeps_the_names_the_file_already_holds() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-keep");
    let home = scratch_home("ws-keep");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );
    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    let path = home.join("workspaces/duo.toml");
    let before = fs::read_to_string(&path).expect("start wrote the workspace");
    assert!(before.contains("label = \"implementer\""), "{before}");

    // No daemon anywhere, so no name can be read. The shape still saves.
    let out = cyclops(&home, &["workspace", "save"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    let after = fs::read_to_string(&path).expect("saved file");
    assert!(after.contains("label = \"implementer\""), "{after}");
    assert!(after.contains("label = \"reviewer\""), "{after}");
    // And the line says what happened, both halves of it.
    assert!(
        text.starts_with("✓ workspace saved · duo · 2 panes · 2 agents"),
        "{text}"
    );
    assert!(text.contains("no names could be read"), "{text}");
    assert!(text.contains("The 2 names already in"), "{text}");
    assert!(text.contains("were kept as they were"), "{text}");

    let _ = fs::remove_dir_all(&home);
}

/// Same loss, the other way in: a daemon that IS watching and has nothing
/// on its roster.
///
/// An empty registry is not the daemon saying these panes have no names.
/// It is the daemon having nothing to say about them, which is the same
/// absence of testimony as no daemon at all. A daemon that just restarted
/// before its sessions reattached is exactly this. Writing "no names" over
/// the file's own leaves them in neither place.
#[test]
fn save_with_a_watching_daemon_and_an_empty_roster_keeps_the_names() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-empty");
    let home = scratch_home("ws-empty");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );
    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    let path = home.join("workspaces/duo.toml");
    let before = fs::read_to_string(&path).expect("start wrote the workspace");
    assert!(before.contains("label = \"implementer\""), "{before}");

    // Watching "duo", attached, and holding no name for either pane. Two
    // connections: the save below, and the --json save after it.
    let ids: Vec<String> = panes(&t, "duo").into_iter().map(|(id, _, _)| id).collect();
    let _seen = canned_daemon(&home, 2, "duo", unnamed(&ids), &[]);

    let out = cyclops(&home, &["workspace", "save"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    let after = fs::read_to_string(&path).expect("saved file");
    assert!(after.contains("label = \"implementer\""), "{after}");
    assert!(after.contains("label = \"reviewer\""), "{after}");

    // The printed line answers for what landed on disk: two names are in
    // the file, so the count is two, and the check is the light one
    // because no roster stood behind that number.
    assert!(
        text.starts_with("✓ workspace saved · duo · 2 panes · 2 agents"),
        "{text}"
    );
    assert!(text.contains("has no names on its roster"), "{text}");
    assert!(text.contains("The 2 names already in"), "{text}");
    assert!(text.contains("were kept as they were"), "{text}");

    // --json says the same thing in one word, so a script branches the
    // same way a person reads.
    let out = cyclops(&home, &["--json", "workspace", "save"]);
    assert!(out.status.success(), "{out:?}");
    let v: Value = serde_json::from_str(stdout(&out).trim()).expect("json");
    assert_eq!(v["names_from"], json!("file"), "{v}");
    assert_eq!(v["agents"], json!(2), "{v}");

    let _ = fs::remove_dir_all(&home);
}

/// The other half of that rule. When the shape moved, the kept names have
/// no pane to sit on, and a file with the geometry right and the roster
/// gone is the loss this verb exists to avoid. So it writes nothing.
#[test]
fn save_without_a_daemon_refuses_when_the_names_have_nowhere_to_go() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-nowhere");
    let home = scratch_home("ws-nowhere");
    write_config(
        &home,
        &t,
        "sessions = [\"duo\"]\ndefault_workspace = \"duo\"\n",
    );
    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    let path = home.join("workspaces/duo.toml");
    let before = fs::read_to_string(&path).expect("start wrote the workspace");
    t.run_ok(&["split-window", "-h", "-d", "-t", "duo:0.0"]);

    let out = cyclops(&home, &["workspace", "save"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("holds 2 names"), "{err}");
    assert!(err.contains("Nothing was written"), "{err}");
    assert!(err.contains("Start cyclopsd and save again"), "{err}");
    // The file is exactly as it was, names and all.
    assert_eq!(fs::read_to_string(&path).expect("still there"), before);

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_workspace_nobody_saved_says_how_to_save_one() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-missing");
    let home = scratch_home("ws-missing");
    write_config(&home, &t, "sessions = [\"ghost\"]\n");
    let out = cyclops(&home, &["workspace", "restore", "ghost"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("no workspace called \"ghost\""), "{err}");
    assert!(err.contains("cyclops workspace save ghost"), "{err}");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn an_unknown_preset_lists_the_ones_that_exist() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-preset");
    let home = scratch_home("ws-preset");
    write_config(&home, &t, "sessions = [\"main\"]\n");
    let out = cyclops(&home, &["start", "--preset", "sextet"]);
    assert_eq!(out.status.code(), Some(2), "usage mistakes exit 2");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("solo, duo, quad, ops"), "{err}");
    let _ = fs::remove_dir_all(&home);
}

/// The whole point of M7: one command, from nothing to a workspace with
/// named panes and a daemon that outlives the shell that started it.
///
/// This is the only test that lets `start` spawn a daemon, so it stops
/// the one it started. Everything else passes --no-daemon (see
/// `cyclops_raw`).
#[test]
fn start_starts_a_daemon_when_none_is_running() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-daemon");
    let home = scratch_home("ws-daemon");
    write_config(
        &home,
        &t,
        "sessions = [\"solo\"]\ndefault_workspace = \"solo\"\n",
    );

    let out = cyclops_with_daemon(&home, &["start"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("started cyclopsd"), "{text}");
    assert!(home.join("cyclopsd.log").is_file(), "no log was written");

    // Heavy check: with a daemon up, the roster is one it confirmed
    // rather than a count read off the workspace file. That is the
    // difference the whole change exists to make.
    assert!(
        text.starts_with("✔ workspace ready"),
        "names did not land in one run: {text}"
    );
    // And no "start the daemon" step, because it is running.
    assert!(!text.contains("cyclopsd &"), "{text}");

    // A second run finds it and says nothing about starting one.
    let again = stdout(&cyclops_with_daemon(&home, &["start"]));
    assert!(
        !again.contains("started cyclopsd"),
        "started a second: {again}"
    );

    // `cyclops daemon status` sees it, and stop takes it down.
    let status = stdout(&cyclops(&home, &["daemon", "status"]));
    assert!(status.contains("cyclopsd is running"), "{status}");

    let stopped = stdout(&cyclops(&home, &["daemon", "stop"]));
    assert!(stopped.contains("stopped cyclopsd"), "{stopped}");
    for _ in 0..50 {
        if !home.join("sock").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let after = stdout(&cyclops(&home, &["daemon", "status"]));
    assert!(after.contains("not running"), "still up: {after}");

    let _ = fs::remove_dir_all(&home);
}
