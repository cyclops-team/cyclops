//! Bare `cyclops` invocation (workspace Step 4).

#![cfg(feature = "full-ui")]

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cyclops_testrig::{tmux_available, TmuxServer};

#[test]
fn bare_non_tty_prints_help_and_exits_zero() {
    let out = Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("help") || text.contains("cyclops"),
        "stdout={text}"
    );
}

/// One pty: (master fd, slave file). Same allocation as
/// tests/terminal_silence.rs, minus its ptsname lock: this binary has one
/// pty caller, so no two threads can race the static buffer.
fn pty() -> (i32, File) {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(
        master >= 0,
        "posix_openpt: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(unsafe { libc::grantpt(master) }, 0, "grantpt");
    assert_eq!(unsafe { libc::unlockpt(master) }, 0, "unlockpt");
    let name = unsafe {
        let p = libc::ptsname(master);
        assert!(!p.is_null(), "ptsname");
        CStr::from_ptr(p).to_string_lossy().into_owned()
    };
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&name)
        .expect("open pty slave");
    (master, slave)
}

/// Scratch CYCLOPS_HOME under the relocatable scratch root (F24).
fn scratch_home(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch home");
    dir
}

/// The tty path seeds the shipped themes and detection manifests before
/// the workspace opens, the same way `cyclops start` does. The live
/// failures this pins: a config or `$CYCLOPS_THEME` naming a shipped theme
/// on a home `start` never touched was a missing themes/light.toml; a
/// binary-only first run with no manifests left every pane unknown and
/// undeliverable.
///
/// The rig goes as far as the tty branch itself: bare `cyclops` on a pty,
/// killed once the seed is on disk. The daemon socket is held by this test
/// so `ensure_daemon` cannot leave a live cyclopsd behind (a spawned one
/// finds the socket held and exits at boot), and the config points tmux at
/// an isolated testrig server in case the boot gets that far.
#[test]
fn bare_tty_seeds_the_shipped_themes_and_manifests() {
    if !tmux_available() {
        return;
    }
    let t = TmuxServer::new("bare-seed");
    let home = scratch_home("bareseed");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "tmux_socket = \"{}\"\ntmux_config = \"/dev/null\"\n",
            t.socket()
        ),
    )
    .expect("write config");
    let _sock = UnixListener::bind(home.join(cyclops_proto::SOCK_NAME)).expect("bind sock");

    let (master, slave) = pty();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", &home)
        .env_remove("CYCLOPS_THEME")
        .env_remove("TMUX")
        .stdin(slave.try_clone().expect("dup pty for stdin"))
        .stdout(slave.try_clone().expect("dup pty for stdout"))
        .stderr(slave.try_clone().expect("dup pty for stderr"))
        .spawn()
        .expect("run cyclops");

    // Themes and manifests are the first things the tty branch writes, so
    // they land well before the daemon step's connect budget runs out.
    // Bounded wait, then stop the workspace; nothing after the seed is
    // asserted on.
    //
    // Wait for every file the assertions below name, not just the first of
    // each kind. Waiting on `claude.toml` alone killed the child mid-seed
    // under a loaded machine: the manifests directory held claude and agy,
    // codex had not been written yet, and the failure read as a seeder bug
    // rather than as this loop letting go too early. Five files instead of
    // two costs nothing when the seed is fast, which is the common case.
    let light = home.join("themes/light.toml");
    let claude = home.join("manifests/claude.toml");
    let seeded_all = || {
        light.is_file()
            && home.join("themes/dark.toml").is_file()
            && claude.is_file()
            && home.join("manifests/codex.toml").is_file()
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seeded_all() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    unsafe { libc::close(master) };

    let seeded: Vec<String> = std::fs::read_dir(home.join("themes"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        light.is_file(),
        "bare cyclops did not seed themes; the dir holds {seeded:?}"
    );
    assert!(home.join("themes/dark.toml").is_file(), "{seeded:?}");
    let manifests: Vec<String> = std::fs::read_dir(home.join("manifests"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(
        claude.is_file(),
        "bare cyclops did not seed manifests; the dir holds {manifests:?}"
    );
    assert!(home.join("manifests/codex.toml").is_file(), "{manifests:?}");
    let _ = std::fs::remove_dir_all(&home);
}
