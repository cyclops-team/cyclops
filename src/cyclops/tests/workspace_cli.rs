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
use std::os::unix::fs::MetadataExt;
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
/// Owns the daemon a test starts, and proves it is gone before removing
/// the scratch home.
///
/// Armed before the daemon can exist: a guard armed after the assertions
/// only runs when they pass. Identity is the daemon's own pid plus kernel
/// birth, captured between two matching status reads. The home is kept
/// when exit cannot be proven, because it is the only evidence of which
/// run leaked.
struct DaemonHome {
    home: PathBuf,
    /// Plain ownership: a guard is local to one scope and never shared,
    /// so a lock here would only add a way for teardown to fail.
    pid: Option<Daemon>,
}

impl DaemonHome {
    fn new(home: &Path) -> DaemonHome {
        DaemonHome {
            home: home.to_path_buf(),
            pid: None,
        }
    }

    /// Record who the daemon is, before any assertion runs.
    ///
    /// Reads the exact `pid` field from `--json`; the human status line
    /// carries other numbers. Birth comes with it, because a pid alone is
    /// a number the kernel reuses.
    /// `by` is the caller's own deadline, and every subprocess below is
    /// capped at what remains of it. Per-call timeouts compose: two ten
    /// second asks inside a thirty second loop is a seventy second wait
    /// wearing a thirty second label.
    fn record_pid(&mut self, by: std::time::Instant) {
        self.pid = next_identity(
            self.pid.take(),
            |d| birth_of(d.pid) == Some(d.birth),
            || {
                let (pid, boot_id) = self.ask(by)?;
                let birth = birth_of(pid)?;
                // Asked again AFTER the birth read, and both answers must
                // name the same pid and run: otherwise a daemon can exit
                // and a replacement take its pid between the two reads,
                // arming this guard against the wrong process.
                (self.ask(by) == Some((pid, boot_id.clone()))).then_some(Daemon {
                    pid,
                    birth,
                    boot_id,
                })
            },
        );
    }

    /// The daemon's own pid and run id, or None when nothing answers.
    fn ask(&self, by: std::time::Instant) -> Option<(u32, String)> {
        let left = by.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        let text = cyclops_bounded(&self.home, &["daemon", "status", "--json"], left)?;
        let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
        Some((
            parsed.get("pid")?.as_u64()? as u32,
            parsed.get("boot_id")?.as_str()?.to_string(),
        ))
    }

    fn recorded(&self) -> Option<Daemon> {
        self.pid.clone()
    }

    /// Is the exact process this guard recorded still alive?
    ///
    /// Birth alone, and deliberately: a daemon can outlive its socket,
    /// and that IS the leak. Whether it still answers is a different
    /// question, already settled by the bracketed capture that recorded
    /// it, and asking again would spend another subprocess to learn less.
    fn alive(&self) -> bool {
        self.recorded()
            .is_some_and(|d| birth_of(d.pid) == Some(d.birth))
    }
}

/// Which identity a capture should end up owning.
///
/// A still-live recorded process WINS: it needs no recapture, and
/// recapturing would hand ownership to whatever the home reports now
/// while the recorded one is still running. Only once that exact birth
/// is dead does the reported daemon take its place, and a capture that
/// fails then leaves nothing behind rather than a dead predecessor
/// standing in for whatever is live.
///
/// Split out so the rule is testable without a daemon.
fn next_identity(
    prior: Option<Daemon>,
    live: impl Fn(&Daemon) -> bool,
    reported: impl FnOnce() -> Option<Daemon>,
) -> Option<Daemon> {
    match prior.filter(live) {
        Some(alive) => Some(alive),
        None => reported(),
    }
}

/// One process, told apart from whatever inherits its number later.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Daemon {
    pid: u32,
    /// Kernel start time, microseconds. Compared, never interpreted.
    birth: u64,
    /// The daemon's own run identity, so the bracket below proves the
    /// SAME daemon answered twice rather than a replacement.
    boot_id: String,
}

/// Kernel start time of a pid, microseconds, or None when it is gone.
/// Second-resolution sources alias two processes inside one second,
/// which is the aliasing this identity exists to prevent.
#[cfg(target_os = "macos")]
fn birth_of(pid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let rc = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(info).cast(),
            size,
        )
    };
    if rc != size {
        return None;
    }
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(target_os = "linux")]
fn birth_of(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn birth_of(_pid: u32) -> Option<u64> {
    None
}

/// Wait for a child with a deadline, killing and reaping it if it
/// overruns. None means it did not finish in time.
///
/// Every wait in this harness goes through here: a blocking `output()`
/// in an ownership path outlives the deadline meant to bound it.
fn wait_bounded(
    mut child: std::process::Child,
    within: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// A cyclops CLI call that cannot outlive its deadline. Only a complete,
/// successful run answers; a timeout is killed, reaped, and reported as
/// nothing.
fn cyclops_bounded(home: &Path, args: &[&str], within: std::time::Duration) -> Option<String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("CYCLOPS_THEME")
        .env_remove("TMUX")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut buf = String::new();
        use std::io::Read as _;
        let _ = out.read_to_string(&mut buf);
        buf
    });
    let status = wait_bounded(child, within)?;
    let text = reader.join().ok()?;
    status.success().then_some(text)
}

impl Drop for DaemonHome {
    fn drop(&mut self) {
        let daemon = self.pid.clone();
        if self.home.join("sock").exists() {
            if let Ok(child) = Command::new(env!("CARGO_BIN_EXE_cyclops"))
                .env("CYCLOPS_HOME", &self.home)
                .env("NO_COLOR", "1")
                .args(["daemon", "stop"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                wait_bounded(child, std::time::Duration::from_secs(5));
            }
        }
        for _ in 0..50 {
            if !self.home.join("sock").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // Exact-pid fallback, and only while that pid is still the
        // daemon: signalling a number the kernel has since handed to
        // something else is the bug this whole session is about.
        if let Some(d) = &daemon {
            if birth_of(d.pid) == Some(d.birth) {
                // Direct signal: another external command inside Drop is
                // another unbounded wait.
                unsafe { libc::kill(d.pid as i32, libc::SIGTERM) };
                for _ in 0..50 {
                    if birth_of(d.pid) != Some(d.birth) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
        // Only once the exact process is proven gone. A guard that never
        // learned an identity has proven nothing and keeps the home.
        let gone = daemon
            .as_ref()
            .is_some_and(|d| birth_of(d.pid) != Some(d.birth));
        if gone {
            let _ = fs::remove_dir_all(&self.home);
        } else {
            eprintln!(
                "LEAK: daemon still up for {}; home kept as evidence",
                self.home.display()
            );
        }
    }
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

/// `cyclops` with the OS home relocated to scratch, for the vendor-wiring
/// tests: the writes behind install consent resolve ~/.claude and
/// ~/.codex from $HOME, so pointing HOME at a directory this test owns is
/// what keeps them off the real one. CODEX_HOME is scrubbed because it
/// overrides that resolution, and CYCLOPS_NO_VENDOR_HOOKS because a
/// developer who set it would change what these tests assert.
fn cyclops_in_user_home(home: &Path, user: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut argv: Vec<&str> = args.to_vec();
    if args.first() == Some(&"start") && !args.contains(&"--setup-only") {
        argv.push("--no-daemon");
    }
    Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .env("HOME", user)
        .env("NO_COLOR", "1")
        .env_remove("CYCLOPS_THEME")
        .env_remove("TMUX")
        .env_remove("CODEX_HOME")
        .env_remove("CYCLOPS_NO_VENDOR_HOOKS")
        .envs(env.iter().map(|(k, v)| (k.to_string(), v.to_string())))
        .args(&argv)
        .output()
        .expect("run cyclops")
}

fn shipped_skill() -> Vec<u8> {
    fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/cyclops/SKILL.md"))
        .expect("read shipped skill")
}

fn released_skill_at(commit: &str) -> Option<Vec<u8>> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let object = format!("{commit}:skills/cyclops/SKILL.md");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show", &object])
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

#[derive(Debug, PartialEq, Eq)]
struct TreeEntry {
    path: PathBuf,
    kind: &'static str,
    mode: u32,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    body: Vec<u8>,
}

fn tree_snapshot(root: &Path) -> Vec<TreeEntry> {
    fn walk(root: &Path, path: &Path, entries: &mut Vec<TreeEntry>) {
        let metadata = fs::symlink_metadata(path).expect("tree metadata");
        let kind = if metadata.is_dir() {
            "dir"
        } else if metadata.is_file() {
            "file"
        } else if metadata.file_type().is_symlink() {
            "symlink"
        } else {
            "other"
        };
        entries.push(TreeEntry {
            path: path
                .strip_prefix(root)
                .expect("path under root")
                .to_path_buf(),
            kind,
            mode: metadata.mode(),
            len: metadata.len(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
            body: if metadata.is_file() {
                fs::read(path).expect("tree file")
            } else {
                Vec::new()
            },
        });
        if metadata.is_dir() {
            let mut children: Vec<PathBuf> = fs::read_dir(path)
                .expect("tree directory")
                .map(|entry| entry.expect("tree entry").path())
                .collect();
            children.sort();
            for child in children {
                walk(root, &child, entries);
            }
        }
    }

    let mut entries = Vec::new();
    walk(root, root, &mut entries);
    entries
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
            let hello = json!({
                "cyclops": "0.1.0",
                "build": env!("CYCLOPS_BUILD_REF"),
                "proto": 1,
                "boot_id": "b-ws"
            });
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

/// FNV-1a 64, hex. Restated here rather than reached for: the fixture
/// below must hash exactly as the receipt writer does, and hashing it
/// independently is what makes the assertion say so.
fn fnv64(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

/// A receipted hook config naming a binary that no longer exists is what
/// an update leaves behind, and the receipt's whole promise — printed by
/// `cyclops hooks install` — is that the next start repairs it.
/// hooks_cli.rs pins what the receipt records; this pins the wiring:
/// setup actually runs the refresh and says what it did.
#[test]
fn setup_only_refreshes_receipted_hook_configs_after_a_bin_move() {
    let home = scratch_home("ws-refresh");
    let out = cyclops(&home, &["hooks", "install", "codex", "--agent", "reviewer"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dir = home.join("hooks/codex/reviewer");
    let artifact = dir.join("hooks.json");
    let receipt_path = dir.join(".cyclops-prepared.json");
    let current_bin = env!("CARGO_BIN_EXE_cyclops");
    let content = fs::read_to_string(&artifact).expect("installed artifact");
    assert!(content.contains(current_bin), "{content}");

    // Rewind to the install an update orphaned: artifact and receipt both
    // name a binary that is gone. The path is scratch-unique, so no file
    // outside this home can accidentally contain it.
    let old_bin = home.join("gone-build/cyclops");
    let old_bin = old_bin.to_str().expect("utf-8 scratch path");
    let stale = content.replace(current_bin, old_bin);
    fs::write(&artifact, &stale).expect("write stale artifact");
    let mut receipt: Value =
        serde_json::from_str(&fs::read_to_string(&receipt_path).expect("receipt"))
            .expect("receipt is JSON");
    receipt["bin"] = json!(old_bin);
    receipt["rendered_fnv"] = json!(fnv64(stale.as_bytes()));
    fs::write(&receipt_path, format!("{receipt}\n")).expect("write stale receipt");

    let out = cyclops(&home, &["start", "--setup-only"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    let refreshed = fs::read_to_string(&artifact).expect("refreshed artifact");
    assert!(
        refreshed.contains(current_bin),
        "the artifact must run this build again: {refreshed}"
    );
    assert!(!refreshed.contains(old_bin), "{refreshed}");
    assert!(text.contains("refreshed 1 prepared hook config"), "{text}");
    assert!(text.contains("they now run this build"), "{text}");
    // The repaired receipt is what keeps the NEXT refresh honest.
    let receipt: Value = serde_json::from_str(&fs::read_to_string(&receipt_path).expect("receipt"))
        .expect("receipt is JSON");
    assert_eq!(receipt["bin"], current_bin);

    // A second run finds everything current and says nothing about hooks.
    let again = stdout(&cyclops(&home, &["start", "--setup-only"]));
    assert!(!again.contains("refreshed"), "{again}");

    let _ = fs::remove_dir_all(&home);
}

/// `--wire-hooks` consent becomes a file, and only actual consent does:
/// neither a bare `--setup-only` nor a declined (`CYCLOPS_NO_VENDOR_HOOKS`)
/// install may leave a marker that later boots would act on.
#[test]
fn wire_hooks_consent_is_recorded_only_when_given() {
    let home = scratch_home("ws-consent");
    let user = scratch_home("ws-consent-user");
    let marker = home.join("vendor-wiring-consented");

    // No flag: nobody consented to vendor-home writes.
    let out = cyclops_in_user_home(&home, &user, &[], &["start", "--setup-only"]);
    assert!(out.status.success(), "{out:?}");
    assert!(!marker.exists(), "a bare setup recorded consent");

    // The flag, declined by the env: declined means declined, durably too.
    let out = cyclops_in_user_home(
        &home,
        &user,
        &[("CYCLOPS_NO_VENDOR_HOOKS", "1")],
        &["start", "--setup-only", "--wire-hooks"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(!marker.exists(), "a declined install recorded consent");

    // The flag alone: recorded, and no vendor dir invented to go with it.
    let out = cyclops_in_user_home(
        &home,
        &user,
        &[],
        &["start", "--setup-only", "--wire-hooks"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(marker.is_file(), "consent was not recorded");
    for path in [
        user.join(".claude"),
        user.join(".codex"),
        user.join(".cursor"),
        user.join(".agents"),
        user.join(".gemini"),
    ] {
        assert!(!path.exists(), "setup invented {}", path.display());
    }

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn setup_seeds_each_installed_consumer_at_its_canonical_skill_path() {
    let home = scratch_home("ws-skills-fresh");
    let user = scratch_home("ws-skills-fresh-user");
    fs::create_dir_all(user.join(".claude")).expect("create Claude home");
    fs::create_dir_all(user.join(".codex")).expect("create Codex home");
    fs::create_dir_all(user.join(".gemini/antigravity-cli")).expect("create AGY home");

    let out = cyclops_in_user_home(
        &home,
        &user,
        &[],
        &["start", "--setup-only", "--wire-hooks"],
    );
    assert!(out.status.success(), "{out:?}");

    let expected = shipped_skill();
    for path in [
        user.join(".claude/skills/cyclops/SKILL.md"),
        user.join(".agents/skills/cyclops/SKILL.md"),
        user.join(".gemini/antigravity-cli/skills/cyclops/SKILL.md"),
    ] {
        assert_eq!(
            fs::read(&path).expect("seeded skill"),
            expected,
            "{}",
            path.display()
        );
    }
    for path in [
        user.join(".codex/skills/cyclops/SKILL.md"),
        user.join(".gemini/skills/cyclops/SKILL.md"),
    ] {
        assert!(!path.exists(), "unexpected duplicate at {}", path.display());
    }

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn agy_only_setup_wires_hooks_without_seeding_the_shared_skill() {
    let home = scratch_home("ws-skills-agy-only");
    let user = scratch_home("ws-skills-agy-only-user");
    fs::create_dir_all(user.join(".gemini/antigravity-cli")).expect("create AGY home");

    let out = cyclops_in_user_home(
        &home,
        &user,
        &[],
        &["start", "--setup-only", "--wire-hooks"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        user.join(".gemini/antigravity-cli/skills/cyclops/SKILL.md")
            .is_file(),
        "AGY skill is missing"
    );
    assert!(
        user.join(".agents/hooks.json").is_file(),
        "AGY hooks are missing"
    );
    assert!(
        !user.join(".agents/skills").exists(),
        "AGY triggered the Codex and Cursor skill"
    );

    let check = cyclops_in_user_home(&home, &user, &[], &["--json", "setup", "check"]);
    assert!(check.status.success(), "{check:?}");
    let report: Value = serde_json::from_slice(&check.stdout).expect("setup check JSON");
    assert_eq!(report["complete"], true, "{report}");
    assert_eq!(report["consumers"][3]["hook"]["state"], "current");
    assert_eq!(report["consumers"][3]["hook"]["required_receipt_tier"], 2);
    assert_eq!(report["consumers"][3]["hook"]["ack_capable"], false);
    assert_eq!(report["consumers"][3]["hook"]["receipt_ready"], true);

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn codex_and_cursor_share_one_skill_seed() {
    let home = scratch_home("ws-skills-shared");
    let user = scratch_home("ws-skills-shared-user");
    fs::create_dir_all(user.join(".codex")).expect("create Codex home");
    fs::create_dir_all(user.join(".cursor")).expect("create Cursor home");

    let out = cyclops_in_user_home(
        &home,
        &user,
        &[],
        &["--json", "start", "--setup-only", "--wire-hooks"],
    );
    assert!(out.status.success(), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).expect("setup JSON");
    let skills = report["skill"].as_array().expect("skill results");
    assert_eq!(skills.len(), 1, "{report}");
    assert_eq!(
        skills[0]["path"],
        user.join(".agents/skills/cyclops/SKILL.md")
            .display()
            .to_string()
    );
    assert!(!user.join(".codex/skills").exists());
    assert!(!user.join(".cursor/skills").exists());

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn relocated_codex_home_drives_wiring_seeding_and_setup_check() {
    let home = scratch_home("ws-skills-relocated-codex");
    let user = scratch_home("ws-skills-relocated-codex-user");
    let codex_home = user.join("vendor-config/codex");
    fs::create_dir_all(&codex_home).expect("create relocated Codex home");
    let codex_home_text = codex_home.to_str().expect("UTF-8 scratch path");
    let env = [("CODEX_HOME", codex_home_text)];

    let out = cyclops_in_user_home(
        &home,
        &user,
        &env,
        &["start", "--setup-only", "--wire-hooks"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(codex_home.join("hooks.json").is_file());
    assert!(user.join(".agents/skills/cyclops/SKILL.md").is_file());
    assert!(!user.join(".codex").exists());
    assert!(!codex_home.join("skills").exists());

    let check = cyclops_in_user_home(&home, &user, &env, &["--json", "setup", "check"]);
    assert!(check.status.success(), "{check:?}");
    let report: Value = serde_json::from_slice(&check.stdout).expect("setup check JSON");
    let codex = &report["consumers"][1];
    assert_eq!(codex["installed"], true, "{codex}");
    assert_eq!(
        codex["hook"]["path"],
        codex_home.join("hooks.json").display().to_string(),
        "{codex}"
    );
    assert_eq!(codex["hook"]["required_receipt_tier"], 1, "{codex}");
    assert_eq!(codex["hook"]["ack_capable"], true, "{codex}");
    assert_eq!(codex["hook"]["receipt_ready"], true, "{codex}");
    assert_eq!(
        codex["skill"]["path"],
        user.join(".agents/skills/cyclops/SKILL.md")
            .display()
            .to_string(),
        "{codex}"
    );
    assert_eq!(codex["skill"]["state"], "current", "{codex}");
    assert_eq!(codex["mailbox"]["doorbell_ready"], true, "{codex}");

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn a_shared_skill_destination_does_not_count_as_an_installed_consumer() {
    let home = scratch_home("ws-skills-no-consumer");
    let user = scratch_home("ws-skills-no-consumer-user");
    let destination = user.join(".agents/skills/cyclops/SKILL.md");
    fs::create_dir_all(destination.parent().expect("skill parent")).expect("create destination");
    fs::write(&destination, b"# existing shared skill\n").expect("write existing skill");

    let out = cyclops_in_user_home(
        &home,
        &user,
        &[],
        &["--json", "start", "--setup-only", "--wire-hooks"],
    );
    assert!(out.status.success(), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).expect("setup JSON");
    assert_eq!(report["skill"].as_array().expect("skill results").len(), 0);
    assert_eq!(
        fs::read(&destination).expect("read existing skill"),
        b"# existing shared skill\n"
    );

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn setup_preserves_edits_at_every_canonical_skill_path() {
    let home = scratch_home("ws-skills-edited");
    let user = scratch_home("ws-skills-edited-user");
    fs::create_dir_all(user.join(".claude")).expect("create Claude home");
    fs::create_dir_all(user.join(".cursor")).expect("create Cursor home");
    fs::create_dir_all(user.join(".gemini/antigravity-cli")).expect("create AGY home");
    let args = ["start", "--setup-only", "--wire-hooks"];
    assert!(cyclops_in_user_home(&home, &user, &[], &args)
        .status
        .success());

    let paths = [
        user.join(".claude/skills/cyclops/SKILL.md"),
        user.join(".agents/skills/cyclops/SKILL.md"),
        user.join(".gemini/antigravity-cli/skills/cyclops/SKILL.md"),
    ];
    for (index, path) in paths.iter().enumerate() {
        fs::write(path, format!("# operator edit {index}\n")).expect("edit skill");
    }

    assert!(cyclops_in_user_home(&home, &user, &[], &args)
        .status
        .success());
    for (index, path) in paths.iter().enumerate() {
        assert_eq!(
            fs::read_to_string(path).expect("read edited skill"),
            format!("# operator edit {index}\n")
        );
    }

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn setup_upgrades_unedited_skill_bytes_from_a_previous_release() {
    let Some(previous) = released_skill_at("a9ba6634f87e14246969fef3c89e704314a1e234") else {
        return;
    };
    assert_eq!(fnv64(&previous), "7ebc1453af11b931");

    let home = scratch_home("ws-skills-upgrade");
    let user = scratch_home("ws-skills-upgrade-user");
    let paths = [
        user.join(".claude/skills/cyclops/SKILL.md"),
        user.join(".agents/skills/cyclops/SKILL.md"),
        user.join(".gemini/antigravity-cli/skills/cyclops/SKILL.md"),
    ];
    fs::create_dir_all(user.join(".claude")).expect("create Claude home");
    fs::create_dir_all(user.join(".codex")).expect("create Codex home");
    fs::create_dir_all(user.join(".gemini/antigravity-cli")).expect("create AGY home");
    for path in &paths {
        fs::create_dir_all(path.parent().expect("skill parent")).expect("create skill parent");
        fs::write(path, &previous).expect("write previous skill");
    }

    let out = cyclops_in_user_home(
        &home,
        &user,
        &[],
        &["start", "--setup-only", "--wire-hooks"],
    );
    assert!(out.status.success(), "{out:?}");
    let current = shipped_skill();
    assert_ne!(previous, current);
    for path in &paths {
        assert_eq!(fs::read(path).expect("read upgraded skill"), current);
    }

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn setup_check_reports_an_incomplete_empty_home_without_writing() {
    let home = scratch_home("ws-setup-check-empty");
    let user = scratch_home("ws-setup-check-empty-user");
    assert!(fs::read_dir(&home).expect("read home").next().is_none());
    assert!(fs::read_dir(&user)
        .expect("read user home")
        .next()
        .is_none());

    let out = cyclops_in_user_home(&home, &user, &[], &["--json", "setup", "check"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).expect("setup check JSON");
    assert_eq!(report["complete"], false, "{report}");
    let consumers = report["consumers"].as_array().expect("consumer rows");
    assert_eq!(
        consumers
            .iter()
            .map(|row| row["id"].as_str().expect("consumer id"))
            .collect::<Vec<_>>(),
        ["claude", "codex", "cursor", "agy"]
    );
    assert_eq!(
        consumers[0]["skill"]["path"],
        user.join(".claude/skills/cyclops/SKILL.md")
            .display()
            .to_string()
    );
    assert_eq!(
        consumers[1]["skill"]["path"],
        user.join(".agents/skills/cyclops/SKILL.md")
            .display()
            .to_string()
    );
    assert_eq!(consumers[2]["skill"]["path"], consumers[1]["skill"]["path"]);
    assert_eq!(
        consumers[3]["skill"]["path"],
        user.join(".gemini/antigravity-cli/skills/cyclops/SKILL.md")
            .display()
            .to_string()
    );
    assert!(fs::read_dir(&home).expect("read home").next().is_none());
    assert!(fs::read_dir(&user)
        .expect("read user home")
        .next()
        .is_none());
}

#[test]
fn setup_check_reports_complete_setup_and_changes_no_metadata() {
    let home = scratch_home("ws-setup-check-complete");
    let user = scratch_home("ws-setup-check-complete-user");
    for consumer in [
        user.join(".claude"),
        user.join(".codex"),
        user.join(".cursor"),
        user.join(".gemini/antigravity-cli"),
    ] {
        fs::create_dir_all(consumer).expect("create consumer home");
    }
    let setup = ["start", "--setup-only", "--wire-hooks"];
    assert!(cyclops_in_user_home(&home, &user, &[], &setup)
        .status
        .success());

    let home_before = tree_snapshot(&home);
    let user_before = tree_snapshot(&user);
    let out = cyclops_in_user_home(&home, &user, &[], &["--json", "setup", "check"]);
    assert!(out.status.success(), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).expect("setup check JSON");
    assert_eq!(report["complete"], true, "{report}");
    let consumers = report["consumers"].as_array().expect("consumer rows");
    for consumer in consumers {
        assert_eq!(consumer["installed"], true, "{consumer}");
        assert_eq!(consumer["manifest"]["state"], "current", "{consumer}");
        assert_eq!(consumer["skill"]["state"], "current", "{consumer}");
        assert_eq!(consumer["mailbox"]["doorbell_ready"], true, "{consumer}");
        assert_eq!(consumer["mailbox"]["transport"], "doorbell", "{consumer}");
    }
    assert_eq!(consumers[0]["hook"]["state"], "current");
    assert_eq!(consumers[0]["hook"]["required_receipt_tier"], 1);
    assert_eq!(consumers[0]["hook"]["ack_capable"], true);
    assert_eq!(consumers[0]["hook"]["receipt_ready"], true);
    assert_eq!(consumers[1]["hook"]["state"], "current");
    assert_eq!(consumers[1]["hook"]["required_receipt_tier"], 1);
    assert_eq!(consumers[1]["hook"]["ack_capable"], true);
    assert_eq!(consumers[1]["hook"]["receipt_ready"], true);
    assert_eq!(consumers[2]["hook"]["state"], "current");
    assert_eq!(consumers[2]["hook"]["required_receipt_tier"], 1);
    assert_eq!(consumers[2]["hook"]["ack_capable"], true);
    assert_eq!(consumers[2]["hook"]["receipt_ready"], true);
    assert_eq!(consumers[3]["hook"]["state"], "current");
    assert_eq!(consumers[3]["hook"]["required_receipt_tier"], 2);
    assert_eq!(consumers[3]["hook"]["ack_capable"], false);
    assert_eq!(consumers[3]["hook"]["receipt_ready"], true);

    let human = cyclops_in_user_home(&home, &user, &[], &["--plain", "setup", "check"]);
    assert!(human.status.success(), "{human:?}");
    let human = stdout(&human);
    assert!(human.contains("required tier 1 · ack capable"), "{human}");
    assert!(human.contains("required tier 2"), "{human}");
    assert_eq!(
        tree_snapshot(&home),
        home_before,
        "setup check wrote the Cyclops home"
    );
    assert_eq!(
        tree_snapshot(&user),
        user_before,
        "setup check wrote the user home"
    );

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn setup_check_refuses_tier_one_manifests_without_ack_capability() {
    for (id, consumer_dir, index) in [
        ("claude", ".claude", 0usize),
        ("codex", ".codex", 1usize),
        ("cursor", ".cursor", 2usize),
    ] {
        let home = scratch_home(&format!("ws-setup-check-no-ack-{id}"));
        let user = scratch_home(&format!("ws-setup-check-no-ack-{id}-user"));
        fs::create_dir_all(user.join(consumer_dir)).expect("create consumer home");
        let setup = ["start", "--setup-only", "--wire-hooks"];
        assert!(cyclops_in_user_home(&home, &user, &[], &setup)
            .status
            .success());

        let manifest_path = home.join(format!("manifests/{id}.toml"));
        let body = fs::read_to_string(&manifest_path).expect("read seeded manifest");
        let without_ack = body
            .lines()
            .filter(|line| {
                let line = line.trim_start();
                let ack_field = [
                    "ack = ",
                    "ack_evidence = ",
                    "ack_payload_field = ",
                    "ack_latency_ms_p50 = ",
                    "ack_latency_ms_p95 = ",
                ]
                .iter()
                .any(|field| line.starts_with(field));
                let claude_candidate_start = id == "claude"
                    && (line.starts_with("turn_start = ")
                        || line.starts_with("turn_start_evidence = "));
                !ack_field && !claude_candidate_start
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&manifest_path, without_ack).expect("remove ack capability");

        let out = cyclops_in_user_home(&home, &user, &[], &["--json", "setup", "check"]);
        assert_eq!(out.status.code(), Some(1), "{id}: {out:?}");
        let report: Value = serde_json::from_slice(&out.stdout).expect("setup check JSON");
        let consumer = &report["consumers"][index];
        assert_eq!(consumer["manifest"]["state"], "edited", "{consumer}");
        assert_eq!(consumer["hook"]["required_receipt_tier"], 1, "{consumer}");
        assert_eq!(consumer["hook"]["ack_capable"], false, "{consumer}");
        assert_eq!(consumer["hook"]["receipt_ready"], false, "{consumer}");
        assert_eq!(report["complete"], false, "{report}");

        let human = cyclops_in_user_home(&home, &user, &[], &["--plain", "setup", "check"]);
        assert_eq!(human.status.code(), Some(1), "{id}: {human:?}");
        assert!(stdout(&human).contains("required tier 1 · ack missing"));

        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&user);
    }
}

#[test]
fn setup_check_reports_direct_fallback_for_an_edited_claim_skill() {
    let home = scratch_home("ws-setup-check-edited-mailbox-skill");
    let user = scratch_home("ws-setup-check-edited-mailbox-skill-user");
    fs::create_dir_all(user.join(".codex")).expect("create Codex home");
    let setup = ["start", "--setup-only", "--wire-hooks"];
    assert!(cyclops_in_user_home(&home, &user, &[], &setup)
        .status
        .success());
    let skill = user.join(".agents/skills/cyclops/SKILL.md");
    fs::write(&skill, b"operator-owned mailbox instructions\n").expect("edit skill");

    let out = cyclops_in_user_home(&home, &user, &[], &["--json", "setup", "check"]);
    assert!(out.status.success(), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).expect("setup check JSON");
    let codex = &report["consumers"][1];
    assert_eq!(codex["skill"]["state"], "edited", "{codex}");
    assert_eq!(codex["mailbox"]["doorbell_ready"], false, "{codex}");
    assert_eq!(codex["mailbox"]["transport"], "direct_payload", "{codex}");
    assert_eq!(
        codex["mailbox"]["capability_path"],
        skill.display().to_string(),
        "{codex}"
    );

    let human = cyclops_in_user_home(&home, &user, &[], &["--plain", "setup", "check"]);
    assert!(stdout(&human).contains("mailbox   direct payload"));

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn setup_check_identifies_missing_installed_consumer_files_without_writing() {
    let home = scratch_home("ws-setup-check-missing");
    let user = scratch_home("ws-setup-check-missing-user");
    fs::create_dir_all(user.join(".codex")).expect("create Codex home");
    fs::write(user.join(".codex/hooks.json"), b"").expect("create empty hooks file");
    fs::write(
        home.join("vendor-wiring-consented"),
        b"test consent marker\n",
    )
    .expect("write consent marker");

    let home_before = tree_snapshot(&home);
    let user_before = tree_snapshot(&user);
    let out = cyclops_in_user_home(&home, &user, &[], &["--json", "setup", "check"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).expect("setup check JSON");
    let codex = &report["consumers"][1];
    assert_eq!(codex["installed"], true, "{codex}");
    assert_eq!(codex["manifest"]["state"], "missing", "{codex}");
    assert_eq!(codex["hook"]["state"], "needs_update", "{codex}");
    assert_eq!(codex["hook"]["required_receipt_tier"], 1, "{codex}");
    assert_eq!(codex["hook"]["ack_capable"], false, "{codex}");
    assert_eq!(codex["hook"]["receipt_ready"], false, "{codex}");
    assert_eq!(codex["skill"]["state"], "missing", "{codex}");
    assert_eq!(tree_snapshot(&home), home_before);
    assert_eq!(tree_snapshot(&user), user_before);

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

#[test]
fn setup_check_keeps_direct_claude_wiring_when_launch_flag_is_missing() {
    let home = scratch_home("ws-setup-check-claude-flag");
    let user = scratch_home("ws-setup-check-claude-flag-user");
    fs::create_dir_all(user.join(".claude")).expect("create Claude home");
    let setup = cyclops_in_user_home(
        &home,
        &user,
        &[],
        &["start", "--setup-only", "--wire-hooks"],
    );
    assert!(setup.status.success(), "{setup:?}");
    let manifest_path = home.join("manifests/claude.toml");
    let manifest = fs::read_to_string(&manifest_path).expect("read Claude manifest");
    fs::write(
        &manifest_path,
        manifest.replace("settings_flag = \"--settings\"\n", ""),
    )
    .expect("edit Claude manifest");

    let home_before = tree_snapshot(&home);
    let user_before = tree_snapshot(&user);
    let out = cyclops_in_user_home(&home, &user, &[], &["--json", "setup", "check"]);
    assert!(out.status.success(), "{out:?}");
    let report: Value = serde_json::from_slice(&out.stdout).expect("setup check JSON");
    let claude = &report["consumers"][0];
    assert_eq!(claude["manifest"]["state"], "edited", "{claude}");
    assert_eq!(claude["hook"]["state"], "current", "{claude}");
    assert_eq!(claude["hook"]["required_receipt_tier"], 1, "{claude}");
    assert_eq!(claude["hook"]["ack_capable"], true, "{claude}");
    assert_eq!(claude["hook"]["receipt_ready"], true, "{claude}");
    assert_eq!(claude["skill"]["state"], "current", "{claude}");
    assert_eq!(tree_snapshot(&home), home_before);
    assert_eq!(tree_snapshot(&user), user_before);

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

/// The install-order gap: cyclops installed before the agent CLIs used to
/// wire nothing and never retry, because the consent lived only in the
/// installer's one run. With the marker on file, an ordinary boot
/// finishes the job the day the CLI appears — and says so, once.
#[test]
fn a_boot_finishes_the_wiring_for_agent_clis_that_appear_after_install() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-deferred");
    let home = scratch_home("ws-deferred");
    let user = scratch_home("ws-deferred-user");
    write_config(
        &home,
        &t,
        "sessions = [\"solo\"]\ndefault_workspace = \"solo\"\n",
    );

    // Install with consent, before any agent CLI exists: the marker
    // lands, nothing else does.
    let out = cyclops_in_user_home(
        &home,
        &user,
        &[],
        &["start", "--setup-only", "--wire-hooks"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(home.join("vendor-wiring-consented").is_file());
    assert!(!user.join(".claude").exists());

    // A boot with no vendor in sight writes nothing and says nothing.
    let out = cyclops_in_user_home(&home, &user, &[], &["start"]);
    assert!(out.status.success(), "{out:?}");
    assert!(!stdout(&out).contains("appeared since install"), "{out:?}");
    assert!(!user.join(".claude").exists(), "a boot invented ~/.claude");

    // Claude Code and codex arrive (their dot-directories appear).
    fs::create_dir_all(user.join(".claude")).expect("create .claude");
    fs::create_dir_all(user.join(".codex")).expect("create .codex");

    // CYCLOPS_NO_VENDOR_HOOKS still declines, consent on file or not.
    let out = cyclops_in_user_home(
        &home,
        &user,
        &[("CYCLOPS_NO_VENDOR_HOOKS", "1")],
        &["start"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        !user.join(".claude/skills").exists() && !user.join(".codex/hooks.json").exists(),
        "the decline env did not decline"
    );

    // The next ordinary boot completes the install's wiring and says so.
    let out = cyclops_in_user_home(&home, &user, &[], &["start"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(
        user.join(".claude/skills/cyclops/SKILL.md").is_file(),
        "the skill never landed: {text}"
    );
    assert!(
        user.join(".agents/skills/cyclops/SKILL.md").is_file(),
        "the shared skill never landed: {text}"
    );
    assert!(
        user.join(".codex/hooks.json").is_file(),
        "codex hooks never landed: {text}"
    );
    assert!(
        user.join(".claude/settings.json").is_file(),
        "Claude hooks never landed: {text}"
    );
    assert!(
        text.contains("Claude Code appeared since install: placed the cyclops skill at"),
        "{text}"
    );
    assert!(
        text.contains("Codex appeared since install: placed the cyclops skill at"),
        "{text}"
    );
    assert!(
        text.contains("codex appeared since install: wired cyclops hooks in"),
        "{text}"
    );
    assert!(
        text.contains("claude appeared since install: wired cyclops hooks in"),
        "{text}"
    );

    // Steady state is silent: everything is current, nothing to say.
    let again = stdout(&cyclops_in_user_home(&home, &user, &[], &["start"]));
    assert!(!again.contains("appeared since install"), "{again}");

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
}

/// Without the marker, a boot must never look at a vendor home: presence
/// of ~/.claude is not consent, and `cyclops start` predates the marker
/// on plenty of machines.
#[test]
fn no_consent_means_a_boot_never_touches_vendor_homes() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-noconsent");
    let home = scratch_home("ws-noconsent");
    let user = scratch_home("ws-noconsent-user");
    write_config(
        &home,
        &t,
        "sessions = [\"solo\"]\ndefault_workspace = \"solo\"\n",
    );
    fs::create_dir_all(user.join(".claude")).expect("create .claude");
    fs::create_dir_all(user.join(".codex")).expect("create .codex");

    let out = cyclops_in_user_home(&home, &user, &[], &["start"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        !user.join(".claude/skills").exists(),
        "a boot without consent wrote the skill"
    );
    assert!(
        !user.join(".codex/hooks.json").exists(),
        "a boot without consent wired codex"
    );

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&user);
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

    // The first start booted a real cyclopsd; without this it outlives
    // the test reparented to launchd, one leaked daemon per suite run.
    let _ = cyclops(&home, &["daemon", "stop"]);
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

/// A manifest whose launch command is `cat`.
///
/// `--agents` starts real programs in real panes, so its tests need a real
/// command: one every machine has, that stays running when a pane hands it
/// a terminal, and that is not a vendor CLI. Shelling out to claude or
/// codex from the suite would need them installed and would cost a session
/// on somebody's account.
fn stand_in_manifest(home: &Path, id: &str) {
    let dir = home.join("manifests");
    fs::create_dir_all(&dir).expect("create manifests dir");
    fs::write(
        dir.join(format!("{id}.toml")),
        format!(
            "[agent]\nid = \"{id}\"\ndisplay_name = \"Stand-in\"\nprocess_names = [\"cat\"]\nlaunch = \"cat\"\n"
        ),
    )
    .expect("write the stand-in manifest");
}

/// What each pane of a session is running, in position order.
fn running(t: &TmuxServer, session: &str) -> Vec<String> {
    let out = t.run(&[
        "list-panes",
        "-s",
        "-t",
        &format!("={session}"),
        "-F",
        "#{pane_left} #{pane_top} #{pane_current_command}",
    ]);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut rows: Vec<(u32, u32, String)> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (
                f[0].parse().unwrap(),
                f[1].parse().unwrap(),
                f[2].to_string(),
            )
        })
        .collect();
    rows.sort_by_key(|(left, top, _)| (*top, *left));
    rows.into_iter().map(|(_, _, cmd)| cmd).collect()
}

/// [`running`], once tmux has caught up with the exec.
///
/// tmux reports the pane's foreground process, and a pane built a
/// millisecond ago can still be reported as the process that is about to
/// become the command. Bounded, and only in the rig: nothing in cyclops
/// polls for this.
fn wait_running(t: &TmuxServer, session: &str, want: &[&str]) -> Vec<String> {
    for _ in 0..50 {
        let now = running(t, session);
        if now == want {
            return now;
        }
        thread::sleep(std::time::Duration::from_millis(50));
    }
    running(t, session)
}

/// [`running`], once the first `n` panes are running `want`.
///
/// The panes after them are not waited on. One left at a shell reports
/// whatever shell the machine has, and what a test asserts about that pane
/// is what it is NOT running.
fn wait_leading(t: &TmuxServer, session: &str, n: usize, want: &str) -> Vec<String> {
    // A wider budget than the file's usual 50 polls, because this waits on
    // panes tmux has to exec a real binary into, not on a state flip. The
    // loop returns the instant the panes arrive, so the ceiling only costs
    // a run that was going to fail anyway; at 50 it failed 2 runs in 4
    // under a loaded machine.
    for _ in 0..200 {
        let now = running(t, session);
        if now.len() >= n && now.iter().take(n).all(|c| c == want) {
            return now;
        }
        thread::sleep(std::time::Duration::from_millis(50));
    }
    running(t, session)
}

/// A saved workspace `--agents` can only half fill: two named panes and a
/// dock, every one of them carrying a command already.
///
/// The dock is the `ops` dock in miniature. It holds a command of its own
/// and no label, so nothing addresses it and `--agents` never writes to it.
/// Its command is deliberately one no manifest here names, so a pane
/// running it can only have got it off disk.
fn workspace_with_a_dock(home: &Path, name: &str) {
    let dir = home.join("workspaces");
    fs::create_dir_all(&dir).expect("create workspaces dir");
    fs::write(
        dir.join(format!("{name}.toml")),
        format!(
            "name = \"{name}\"\n\
             \n\
             [[windows]]\n\
             name = \"{name}\"\n\
             \n\
             [[windows.rows]]\n\
             ratio = 0.7\n\
             \n\
             [[windows.rows.panes]]\n\
             label = \"implementer\"\n\
             ratio = 0.5\n\
             command = \"sleep 300\"\n\
             \n\
             [[windows.rows.panes]]\n\
             label = \"reviewer\"\n\
             ratio = 0.5\n\
             command = \"sleep 300\"\n\
             \n\
             [[windows.rows]]\n\
             ratio = 0.3\n\
             \n\
             [[windows.rows.panes]]\n\
             ratio = 1.0\n\
             command = \"sleep 300\"\n"
        ),
    )
    .expect("write the workspace");
}

/// The input path a shipped preset deliberately leaves blank: which CLI
/// runs in which pane. Naming them fills the panes it builds AND the file
/// it writes, so the fleet is part of the workspace from then on.
#[test]
fn naming_agents_starts_them_and_writes_them_into_the_workspace() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-agents");
    let home = scratch_home("ws-agents");
    write_config(
        &home,
        &t,
        "sessions = [\"main\"]\ndefault_workspace = \"main\"\n",
    );
    stand_in_manifest(&home, "demo");

    let out = cyclops(
        &home,
        &["start", "--preset", "duo", "--agents", "demo,demo"],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout(&out).starts_with("✓ workspace ready · 2 agents"),
        "{}",
        stdout(&out)
    );

    // Both panes are running the CLI, in this same invocation and with no
    // --launch: naming them at the keyboard is the decision to run them.
    assert_eq!(wait_running(&t, "main", &["cat", "cat"]), ["cat", "cat"]);

    // And the workspace file carries them, so the arrangement and the
    // fleet are one thing from here on.
    let saved = fs::read_to_string(home.join("workspaces/main.toml")).expect("start saved it");
    assert_eq!(saved.matches("command = \"cat\"").count(), 2, "{saved}");

    let _ = fs::remove_dir_all(&home);
}

/// The other half of that rule, and the one that is easy to break.
///
/// A command written into a workspace is a suggestion; running it stays an
/// explicit choice per run (cyclops_tmux::layout, push_pane_args). Naming
/// agents does not weaken that: the next `cyclops start` over the same file
/// opens shells, and only `--launch` replays what is written there.
#[test]
fn a_later_start_over_that_workspace_still_needs_launch() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-agents-again");
    let home = scratch_home("ws-agents-again");
    write_config(
        &home,
        &t,
        "sessions = [\"main\"]\ndefault_workspace = \"main\"\n",
    );
    stand_in_manifest(&home, "demo");
    assert!(cyclops(
        &home,
        &["start", "--preset", "duo", "--agents", "demo,demo"]
    )
    .status
    .success());
    assert_eq!(wait_running(&t, "main", &["cat", "cat"]), ["cat", "cat"]);

    // Naming them again over the open session starts nothing and says so:
    // `start` never touches a session that exists.
    let again = cyclops(&home, &["start", "--agents", "demo,demo"]);
    assert!(again.status.success(), "{again:?}");
    assert!(
        stdout(&again).contains("was already open, so --agents started nothing"),
        "{}",
        stdout(&again)
    );

    // The session goes away, and a bare start rebuilds it as shells.
    t.run_ok(&["kill-session", "-t", "=main"]);
    assert!(cyclops(&home, &["start"]).status.success());
    let plain = running(&t, "main");
    assert!(
        plain.iter().all(|c| c != "cat"),
        "a bare start replayed the stored commands: {plain:?}"
    );

    // And --launch is what runs them, exactly as it does for any other
    // command a workspace file holds.
    t.run_ok(&["kill-session", "-t", "=main"]);
    assert!(cyclops(&home, &["start", "--launch"]).status.success());
    assert_eq!(wait_running(&t, "main", &["cat", "cat"]), ["cat", "cat"]);

    let _ = fs::remove_dir_all(&home);
}

/// A workspace you saved is yours.
///
/// `--agents` over one runs the CLIs it names in the panes it builds and
/// leaves the file exactly as it was: `start` never rewrites a saved
/// workspace, and a flag on one run is not an edit to your record. The
/// file it does write is the one it built from a preset a moment earlier,
/// which is cyclops's own copy either way.
#[test]
fn agents_over_a_saved_workspace_run_without_rewriting_it() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-agents-saved");
    let home = scratch_home("ws-agents-saved");
    write_config(
        &home,
        &t,
        "sessions = [\"main\"]\ndefault_workspace = \"main\"\n",
    );
    stand_in_manifest(&home, "demo");

    // A workspace on disk with no commands in it, and no session.
    assert!(cyclops(&home, &["start", "--preset", "duo"])
        .status
        .success());
    let path = home.join("workspaces/main.toml");
    let before = fs::read_to_string(&path).expect("start saved it");
    assert!(!before.contains("command ="), "{before}");
    t.run_ok(&["kill-session", "-t", "=main"]);

    assert!(cyclops(&home, &["start", "--agents", "demo,demo"])
        .status
        .success());
    assert_eq!(wait_running(&t, "main", &["cat", "cat"]), ["cat", "cat"]);
    assert_eq!(
        fs::read_to_string(&path).expect("still there"),
        before,
        "--agents rewrote a workspace file"
    );

    let _ = fs::remove_dir_all(&home);
}

/// The rule that makes `--agents` safe over a workspace that already holds
/// commands: it runs the ones it just filled, and only those.
///
/// A pane with no label is not part of the fleet, and the command in the
/// file for it is a suggestion like any other: it waits for `--launch`.
/// Both halves are asserted in one run, because that is the run where they
/// used to disagree.
#[test]
fn agents_run_what_they_filled_and_leave_the_dock_alone() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-agents-dock");
    let home = scratch_home("ws-agents-dock");
    write_config(
        &home,
        &t,
        "sessions = [\"main\"]\ndefault_workspace = \"main\"\n",
    );
    stand_in_manifest(&home, "demo");
    workspace_with_a_dock(&home, "main");

    let out = cyclops(&home, &["start", "--agents", "demo,demo"]);
    assert!(out.status.success(), "{out:?}");

    // The two named panes run the CLI that was named for them, in this
    // invocation and with no --launch.
    let now = wait_leading(&t, "main", 2, "cat");
    assert_eq!(now.len(), 3, "the dock is still a pane: {now:?}");
    assert_eq!(now[..2], ["cat", "cat"], "{now:?}");

    // And the dock never becomes its stored command. Sampled over a window
    // rather than once: a pane tmux built with a command reaches it a beat
    // after the pane exists, so one early read would pass either way.
    for _ in 0..10 {
        let now = running(&t, "main");
        assert!(
            !now.iter().any(|c| c == "sleep"),
            "--agents replayed a command it did not write: {now:?}"
        );
        thread::sleep(std::time::Duration::from_millis(50));
    }

    let _ = fs::remove_dir_all(&home);
}

/// The other half of the same rule, over the same file: `--launch` still
/// means every command the workspace holds, the dock's included. Narrowing
/// it to the panes an agent is named for would be the same bug pointed the
/// other way.
#[test]
fn launch_still_runs_every_stored_command_including_the_docks() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-launch-dock");
    let home = scratch_home("ws-launch-dock");
    write_config(
        &home,
        &t,
        "sessions = [\"main\"]\ndefault_workspace = \"main\"\n",
    );
    stand_in_manifest(&home, "demo");
    workspace_with_a_dock(&home, "main");

    let out = cyclops(&home, &["start", "--launch"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        wait_running(&t, "main", &["sleep", "sleep", "sleep"]),
        ["sleep", "sleep", "sleep"],
        "--launch is the whole workspace, dock and all"
    );

    let _ = fs::remove_dir_all(&home);
}

/// Every `--agents` refusal happens before anything is built. A session
/// half opened around a typo is worse than no session: it is one the
/// operator has to take down before trying again.
#[test]
fn agents_that_cannot_be_placed_build_nothing_and_say_what_would_fit() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("ws-agents-no");
    let home = scratch_home("ws-agents-no");
    write_config(
        &home,
        &t,
        "sessions = [\"main\"]\ndefault_workspace = \"main\"\n",
    );
    stand_in_manifest(&home, "demo");

    let short = cyclops(&home, &["start", "--preset", "duo", "--agents", "demo"]);
    assert_eq!(short.status.code(), Some(2), "usage mistakes exit 2");
    let err = String::from_utf8_lossy(&short.stderr).to_string();
    assert!(err.contains("preset duo has 2 named panes"), "{err}");
    assert!(err.contains("--preset solo, which has 1"), "{err}");

    let typo = cyclops(
        &home,
        &["start", "--preset", "duo", "--agents", "dmeo,demo"],
    );
    assert_eq!(typo.status.code(), Some(2));
    let err = String::from_utf8_lossy(&typo.stderr).to_string();
    assert!(err.contains("no agent CLI called \"dmeo\""), "{err}");
    assert!(err.contains("demo"), "the ids that would work: {err}");

    // Neither run left anything behind.
    assert!(
        !t.run(&["has-session", "-t", "=main"]).status.success(),
        "a refused start built a session"
    );
    assert!(!home.join("workspaces/main.toml").exists());

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
    // Armed before anything can spawn: every exit from here on, panic
    // included, takes the daemon down with it.
    let mut _daemon_home = DaemonHome::new(&home);
    write_config(
        &home,
        &t,
        "sessions = [\"solo\"]\ndefault_workspace = \"solo\"\n",
    );

    // Every call here is bounded: this is the one test that owns a real
    // daemon, and a hung CLI would let the runner kill the test before
    // its guard could run.
    let secs = std::time::Duration::from_secs(30);
    let text = cyclops_bounded(&home, &["start"], secs);
    // Identity first, and regardless of how the start went: a CLI that
    // stalled after spawning the daemon would otherwise leave teardown
    // with nothing to signal.
    _daemon_home.record_pid(std::time::Instant::now() + secs);
    let text = text.expect("start");
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
    let again = cyclops_bounded(&home, &["start"], secs).expect("second start");
    assert!(
        !again.contains("started cyclopsd"),
        "started a second: {again}"
    );

    // `cyclops daemon status` sees it, and stop takes it down.
    let status = cyclops_bounded(&home, &["daemon", "status"], secs).expect("status");
    assert!(status.contains("cyclopsd is running"), "{status}");

    let stopped = cyclops_bounded(&home, &["daemon", "stop"], secs).expect("stop");
    assert!(stopped.contains("stopped cyclopsd"), "{stopped}");
    for _ in 0..50 {
        if !home.join("sock").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let after = cyclops_bounded(&home, &["daemon", "status"], secs).expect("status");
    assert!(after.contains("not running"), "still up: {after}");
}

/// A capture that fails must not leave a dead predecessor standing in
/// for whatever is running now.
///
/// Trace: the parent records daemon A; A exits and B comes up under the
/// same home; B's bracket fails transiently. Keeping A means teardown
/// later proves A gone, calls that a clean exit, and removes the home
/// while B is still running.
#[test]
fn a_dead_recorded_daemon_is_not_kept() {
    let a = Daemon {
        pid: 4242,
        birth: 100,
        boot_id: "run-a".to_string(),
    };
    let b = || {
        Some(Daemon {
            pid: 4242,
            birth: 200,
            boot_id: "run-b".to_string(),
        })
    };
    // A live recorded process keeps ownership even while the home reports
    // a different daemon: overwriting it would leave A running with
    // nobody owning it, and teardown would prove only B exited.
    assert_eq!(next_identity(Some(a.clone()), |_| true, b), Some(a.clone()));
    // Dead: the reported daemon takes its place.
    assert_eq!(next_identity(Some(a.clone()), |_| false, b), b());
    // Dead, and nothing reported: nothing recorded, so teardown cannot
    // claim an exit it never observed.
    assert_eq!(next_identity(Some(a), |_| false, || None), None);
    assert_eq!(next_identity(None, |_| true, b), b());
}

/// The guard has to run when the test does NOT: the leak was a daemon
/// stopped only after every assertion, so one timeout mid-test left it
/// running.
#[test]
fn a_panicking_test_still_takes_its_daemon_down() {
    if !tmux_available() {
        return;
    }
    if let Ok(home) = std::env::var(LEAK_CHILD) {
        let hs = std::env::var(LEAK_HANDSHAKE).expect("child needs a handshake dir");
        leak_child(Path::new(&home), Path::new(&hs));
        return;
    }
    leak_case("ws-panic");
}

/// One leak case: the child does something fatal to itself, and the
/// daemon it started has to be gone afterwards.
///
/// The component under test runs in a SUBPROCESS so its failure cannot
/// take the observer with it. That covers a hang as well as a panic: an
/// in-process guard that wedges in `Drop` never reaches any outer
/// cleanup.
fn leak_case(tag: &str) {
    let t = TmuxServer::new(tag);
    let home = scratch_home(tag);
    write_config(
        &home,
        &t,
        "sessions = [\"solo\"]\ndefault_workspace = \"solo\"\n",
    );

    // The PARENT owns the same scratch home. Ownership cannot depend on
    // the child's stdout or its exit: if the guard under test hangs, the
    // child is killed with its pipe undrained, and a parent that learned
    // the identity only from that pipe would have nothing to clean up.
    let mut watcher = DaemonHome::new(&home);
    // A directory of its own, owned by the parent. Inside the daemon home
    // the child's teardown would delete `seen` before the parent could
    // read it; beside the home they would survive every failing path.
    let hs = HandshakeDir::new(&home);
    let ready = hs.0.join("ready");
    let go = hs.0.join("go");
    let abort = hs.0.join("abort");
    let seen_go = hs.0.join("seen");
    let _ = fs::remove_file(&ready);
    let _ = fs::remove_file(&go);
    let child = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "a_panicking_test_still_takes_its_daemon_down"])
        .args(["--nocapture", "--test-threads", "1"])
        .env(LEAK_CHILD, &home)
        .env(LEAK_HANDSHAKE, &hs.0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the panicking child");
    // Child::drop neither kills nor waits, so an assertion unwinding
    // before the wait below would leave this process running.
    let child = ChildGuard(Some(child));

    // A handshake, not a poll. The child holds still after starting its
    // daemon, so the parent is guaranteed to observe it: a correct guard
    // can otherwise tear the daemon down before any poll sees it, and the
    // test would fail as "never started" for being too good.
    //
    // The parent keeps trying to identify the daemon while it waits, so a
    // child that dies before signalling is still owned. No assertion runs
    // until the child has been reaped.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !ready.exists() && std::time::Instant::now() < deadline {
        watcher.record_pid(deadline);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let signalled = fs::read_to_string(&ready).unwrap_or_default();
    watcher.record_pid(deadline);
    let parent_saw = watcher.recorded();
    // Both sides identified the daemon independently, and all three parts
    // have to agree. A marker file plus a later status read would prove
    // only that SOME daemon was up at some point.
    // Both halves: the two sides named the same daemon, and that process
    // is still alive. Text agreement alone would release the child over
    // an identity that had already gone away.
    let agreed = parent_saw
        .as_ref()
        .is_some_and(|d| signalled.trim() == format!("{} {} {}", d.pid, d.birth, d.boot_id))
        && watcher.alive();
    // Two controls, never a kill. Continue asks for the forced panic;
    // abort asks the child to unwind through its OWN daemon guard. A
    // child killed outright would skip that guard and orphan the daemon
    // the parent has just failed to identify, which is the opposite of
    // what this test is for.
    let _ = fs::write(if agreed { &go } else { &abort }, "1");
    let status = wait_bounded(child.take(), std::time::Duration::from_secs(60));
    assert!(!signalled.is_empty(), "the child never signalled a daemon");
    assert!(agreed, "the two sides identified different daemons");
    let Some(status) = status else {
        panic!("the child never exited: the guard's own teardown hangs");
    };
    assert!(
        seen_go.exists(),
        "the child never observed continue, so the forced panic was not the case under test"
    );
    let Some(d) = parent_saw else {
        panic!("the parent could not identify the daemon the child started");
    };
    // A child that skipped its panic proves nothing.
    assert!(
        !status.success(),
        "the child exited cleanly, so no unwind was exercised"
    );

    // The exact process has to be gone, and the guard under test is the
    // only thing that could have done it.
    let gone_by = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while birth_of(d.pid) == Some(d.birth) && std::time::Instant::now() < gone_by {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        birth_of(d.pid) != Some(d.birth),
        "the daemon outlived the test that started it: {d:?}"
    );
    assert!(!home.join("sock").exists(), "socket left behind");
}

/// Owns a spawned child so no unwind leaves it running: `Child` neither
/// kills nor waits on drop.
struct ChildGuard(Option<std::process::Child>);

impl ChildGuard {
    fn take(mut self) -> std::process::Child {
        self.0.take().expect("child taken once")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Env var that switches this test binary into the child role.
const LEAK_CHILD: &str = "CYCLOPS_TEST_LEAK_CHILD";
/// Where the two sides leave their handshake files.
const LEAK_HANDSHAKE: &str = "CYCLOPS_TEST_LEAK_HANDSHAKE";

/// Parent-owned handshake directory, removed on every path.
struct HandshakeDir(PathBuf);

impl HandshakeDir {
    fn new(home: &Path) -> HandshakeDir {
        let dir = home.with_extension("handshake");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("handshake dir");
        HandshakeDir(dir)
    }
}

impl Drop for HandshakeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The child: start a daemon under the guard, report what it started, and
/// die without cleaning up. Everything after the panic is the guard's job.
fn leak_child(home: &Path, hs: &Path) {
    let mut guard = DaemonHome::new(home);
    let started = cyclops_bounded(home, &["start"], std::time::Duration::from_secs(30));
    // Identity before the first assertion: a guard that learns nothing
    // before something fails is the gap itself.
    guard.record_pid(std::time::Instant::now() + std::time::Duration::from_secs(30));
    assert!(started.is_some(), "the child could not start a daemon");
    // Hold still until the parent has taken ownership, then die. The
    // signal carries the identity this guard armed, so the parent can
    // prove both sides captured the same run.
    let armed = guard.recorded().expect("the child guard armed no identity");
    // Written then renamed, so the parent never observes a half-created
    // file: `fs::write` creates before it fills, and a reader that
    // catches that instant sees an empty signal and concludes no daemon
    // was ever started.
    let tmp = hs.join("ready.partial");
    fs::write(
        &tmp,
        format!("{} {} {}", armed.pid, armed.birth, armed.boot_id),
    )
    .expect("stage ready");
    fs::rename(&tmp, hs.join("ready")).expect("signal ready");
    let go = hs.join("go");
    let abort = hs.join("abort");
    let by = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !go.exists() && !abort.exists() && std::time::Instant::now() < by {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if abort.exists() {
        // The parent could not take ownership. Leave the way any ordinary
        // test leaves: through this child's own daemon guard.
        return;
    }
    if !go.exists() {
        // A different ending on purpose, so a broken continue path cannot
        // pass by looking like the case under test.
        panic!("the parent never released this child");
    }
    // Proof this child reached the forced panic through the handshake.
    fs::write(hs.join("seen"), "go").expect("record continue");
    panic!("forced: this stands in for a timed-out assertion");
}
