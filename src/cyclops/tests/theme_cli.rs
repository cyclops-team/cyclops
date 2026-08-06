//! `cyclops theme` end to end: the real binary against a scratch home.
//!
//! No tmux and no real daemon. What this verb does is read a directory,
//! rewrite one config line, and print a preview, and the one thing it asks
//! a daemon for is immediacy, which shows up as a check glyph. The canned
//! socket below is there to prove that difference and nothing else.
//!
//! The listing runs with `NO_COLOR` so the rows can be compared word for
//! word. That is not a weaker check than color: every cell pairs a glyph
//! with a word, and the paint is pinned token by token in the binary's own
//! swatch goldens.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

/// Scratch CYCLOPS_HOME under the relocatable scratch root (F24).
fn scratch_home(tag: &str) -> PathBuf {
    let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("themes")).expect("create scratch home");
    dir
}

/// A theme file setting one visible token. Enough to load and to be told
/// apart; what each token paints is the engine's business and is pinned
/// there.
fn write_theme(home: &Path, name: &str, dim: &str) {
    fs::write(
        home.join(format!("themes/{name}.toml")),
        format!("[surface]\ndim = \"{dim}\"\n"),
    )
    .expect("write theme");
}

fn cyclops(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .env("CYCLOPS_HOME", home)
        .env("NO_COLOR", "1")
        .env_remove("CYCLOPS_THEME")
        .args(args)
        .output()
        .expect("run cyclops")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn config(home: &Path) -> String {
    fs::read_to_string(home.join("config.toml")).expect("read config")
}

/// A daemon that answers `theme.reload`, recording what it was asked.
/// `conns` sequential connections, one per cyclops run.
///
/// `painting` is the theme the answer names, which is the daemon's real
/// contract: it re-resolves the selection itself and reports what it is
/// now painting, not what it was told to paint. Naming a theme other than
/// the one just chosen is a reload that did not reach the screen.
fn canned_daemon(home: &Path, conns: usize, painting: &str) -> Arc<Mutex<Vec<Value>>> {
    let painting = painting.to_string();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(home.join("sock")).expect("bind scratch socket");
    let record = Arc::clone(&seen);
    thread::spawn(move || {
        for _ in 0..conns {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut w = stream;
            let hello = json!({"cyclops": "0.1.0", "proto": 1, "boot_id": "b-th"});
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
                let reply = json!({"id": req["id"], "result": {"theme": painting}});
                if writeln!(w, "{reply}").is_err() {
                    break;
                }
            }
        }
    });
    seen
}

/// The listing: one row per theme, sorted, each row the state groups in
/// that theme's colors, and a marker on the one that is on.
#[test]
fn the_listing_shows_every_theme_and_marks_the_active_one() {
    let home = scratch_home("theme-list");
    write_theme(&home, "dark", "#777777");
    write_theme(&home, "light", "#6e6e6e");
    write_theme(&home, "high-contrast", "#bcbcbc");

    let out = cyclops(&home, &["theme"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    let rows: Vec<&str> = text.lines().map(str::trim_end).collect();
    // Sorted by name, and dark is on because no config key picked another.
    assert_eq!(
        rows[0],
        "▸ dark           ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉"
    );
    assert_eq!(
        rows[1],
        "  high-contrast  ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉"
    );
    assert_eq!(
        rows[2],
        "  light          ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉"
    );
    // Empty states invite the next action.
    assert!(
        stdout(&out).contains("cyclops theme <name> to switch"),
        "{rows:?}"
    );

    // Nothing to list is said, not left blank.
    let bare = scratch_home("theme-bare");
    fs::remove_dir_all(bare.join("themes")).expect("drop themes dir");
    let out = cyclops(&bare, &["theme"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("No themes found"), "{}", stdout(&out));
    assert!(stdout(&out).contains("built-in colors"), "{}", stdout(&out));
}

/// A theme file that will not load is not offered. The listing is an offer
/// to switch, and a row for a file that would come up as built-in colors
/// is a lie the reader only finds out about later.
#[test]
fn a_broken_theme_file_is_not_offered() {
    let home = scratch_home("theme-broken");
    write_theme(&home, "dark", "#777777");
    fs::write(home.join("themes/half.toml"), "[surface\n").expect("write broken theme");
    let out = cyclops(&home, &["theme"]);
    assert!(!stdout(&out).contains("half"), "{}", stdout(&out));
    assert!(stdout(&out).contains("dark"), "{}", stdout(&out));
}

/// The switch: the key is written, the swatch of what was chosen is
/// printed, and the next command is already on it.
#[test]
fn switching_writes_the_key_and_the_next_command_reads_it() {
    let home = scratch_home("theme-set");
    write_theme(&home, "dark", "#777777");
    write_theme(&home, "light", "#6e6e6e");
    fs::write(
        home.join("config.toml"),
        "# mine\nsessions = [\"main\"]\ntheme = \"dark\"\n",
    )
    .expect("seed config");

    let out = cyclops(&home, &["theme", "light"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    let rows: Vec<&str> = text.lines().map(str::trim_end).collect();
    // Light check: no daemon answered, so nothing repainted yet, and the
    // line says when it will.
    assert_eq!(rows[0], "✓ theme light");
    assert_eq!(
        rows[1],
        "  light  ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉"
    );
    assert!(rows[2].contains("the next command picks it up"), "{rows:?}");

    // One line changed. The comment and the other key are untouched: the
    // file belongs to whoever wrote it.
    assert_eq!(
        config(&home),
        "# mine\nsessions = [\"main\"]\ntheme = \"light\"\n"
    );
    // And the switch is what the next command renders.
    let out = cyclops(&home, &["theme"]);
    let text = stdout(&out);
    let marked: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with('▸'))
        .map(str::trim_end)
        .collect();
    assert_eq!(marked.len(), 1, "{text:?}");
    assert!(marked[0].contains("light"), "{marked:?}");
}

/// A name that resolves to nothing changes nothing. A config key pointing
/// at a missing file renders built-in colors and only says so at the next
/// command, which is the failure this refusal exists to prevent.
#[test]
fn a_theme_that_will_not_load_is_refused_and_changes_nothing() {
    let home = scratch_home("theme-ghost");
    write_theme(&home, "dark", "#777777");
    let before = "sessions = [\"main\"]\ntheme = \"dark\"\n";
    fs::write(home.join("config.toml"), before).expect("seed config");

    for bad in ["ghost", "half"] {
        if bad == "half" {
            fs::write(home.join("themes/half.toml"), "[surface\n").expect("write broken theme");
        }
        let out = cyclops(&home, &["theme", bad]);
        assert_eq!(out.status.code(), Some(2), "{bad}: {}", stdout(&out));
        assert!(
            stderr(&out).contains("Nothing was changed"),
            "{bad}: {}",
            stderr(&out)
        );
        assert_eq!(config(&home), before, "{bad} edited the config");
    }
}

/// The daemon's half, which is immediacy and nothing else: it answered
/// naming the theme just chosen, so the borders and any running stream are
/// already on it, and the line wears the heavy check that says a component
/// confirmed it.
#[test]
fn a_running_daemon_makes_the_switch_immediate() {
    let home = scratch_home("theme-live");
    write_theme(&home, "dark", "#777777");
    write_theme(&home, "light", "#6e6e6e");
    let seen = canned_daemon(&home, 1, "light");

    let out = cyclops(&home, &["theme", "light"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    let rows: Vec<&str> = text.lines().map(str::trim_end).collect();
    assert_eq!(rows[0], "✔ theme light");
    assert!(!text.contains("the next command picks it up"), "{rows:?}");
    let asked: Vec<String> = seen
        .lock()
        .expect("record")
        .iter()
        .filter_map(|r| r["method"].as_str().map(str::to_string))
        .collect();
    assert_eq!(asked, vec!["theme.reload".to_string()]);
}

/// The heavy check is a claim about the screen, and only the daemon can
/// answer for the screen. It answers by naming the theme it is painting
/// now, because it re-resolves the selection itself: `CYCLOPS_THEME` in
/// its environment wins over the config key for the life of the process,
/// and a bare name resolves against `./themes` relative to ITS working
/// directory. Either one leaves the borders exactly where they were while
/// the reload returns without an error.
#[test]
fn a_daemon_that_did_not_repaint_gets_no_heavy_check() {
    let home = scratch_home("theme-refused");
    write_theme(&home, "dark", "#777777");
    write_theme(&home, "light", "#6e6e6e");
    // The switch is to light; the daemon says it is still on dark.
    let seen = canned_daemon(&home, 2, "dark");

    let out = cyclops(&home, &["theme", "light"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    let rows: Vec<&str> = text.lines().map(str::trim_end).collect();

    // Not the heavy check, and not the light one either: a check answers
    // for a fact, and this one was contradicted rather than unconfirmed.
    assert_eq!(rows[0], "⚠ theme light · saved, not live", "{rows:?}");
    // What did change, and what did not.
    assert!(
        text.contains("cyclopsd is still painting dark"),
        "the line must name what is on screen: {rows:?}"
    );
    assert!(
        text.contains("pane borders did not change"),
        "the line must say what did not change: {rows:?}"
    );
    assert!(
        text.contains("restart"),
        "the line must carry a next step: {rows:?}"
    );
    // "the next command picks it up" belongs to a down daemon. A daemon
    // that is up and painting something else will not pick anything up.
    assert!(!text.contains("the next command picks it up"), "{rows:?}");
    // The config was still written, which is what "saved" claims.
    assert_eq!(config(&home), "theme = \"light\"\n");

    // Scripts get the same answer. `live` keeps the meaning it had, and
    // the daemon's own answer rides along additively for a caller that
    // wants to say which theme is on the borders.
    let out = cyclops(&home, &["--json", "theme", "light"]);
    let v: Value = serde_json::from_str(stdout(&out).trim()).expect("json switch");
    assert_eq!(v["theme"], json!("light"));
    assert_eq!(v["live"], json!(false));
    assert_eq!(v["daemon_theme"], json!("dark"));

    assert_eq!(seen.lock().expect("record").len(), 2);
}

/// The daemon names the theme's own `name` key, which is not always the
/// file stem the user typed. Comparing against the stem would call a
/// perfectly good repaint a refusal.
#[test]
fn the_daemon_answer_is_matched_against_the_themes_own_name() {
    let home = scratch_home("theme-stemname");
    write_theme(&home, "dark", "#777777");
    fs::write(
        home.join("themes/mine.toml"),
        "name = \"midnight\"\n[surface]\ndim = \"#6e6e6e\"\n",
    )
    .expect("write theme");
    let _seen = canned_daemon(&home, 1, "midnight");

    let out = cyclops(&home, &["theme", "mine"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    let rows: Vec<&str> = text.lines().map(str::trim_end).collect();
    assert_eq!(rows[0], "✔ theme mine", "{rows:?}");
}

/// A theme file that parses but sets no token at all is not a theme. Every
/// token resolves off the compiled default table, so the listing would
/// offer a row painted in built-in colors under somebody's chosen name and
/// picking it would repaint nothing. Broken TOML was the only thing
/// filtered, and broken TOML is the failure that essentially never
/// happens; a file whose token names are all stale does.
#[test]
fn a_theme_that_sets_no_tokens_is_neither_offered_nor_taken() {
    let home = scratch_home("theme-empty");
    write_theme(&home, "dark", "#777777");
    // Three ways to parse and still paint nothing: empty, name only, and
    // every token name stale (the per-state names from before the groups).
    fs::write(home.join("themes/blank.toml"), "").expect("write blank");
    fs::write(home.join("themes/titled.toml"), "name = \"titled\"\n").expect("write titled");
    fs::write(
        home.join("themes/stale.toml"),
        "[state]\nidle = \"#111111\"\nworking = \"#222222\"\n",
    )
    .expect("write stale");
    let before = "theme = \"dark\"\n";
    fs::write(home.join("config.toml"), before).expect("seed config");

    let out = cyclops(&home, &["theme"]);
    let text = stdout(&out);
    for gone in ["blank", "titled", "stale"] {
        assert!(!text.contains(gone), "{gone} is offered: {text}");
    }
    assert!(text.contains("dark"), "{text}");

    // Scripts read the same listing, so they see the same offer.
    let out = cyclops(&home, &["--json", "theme"]);
    let v: Value = serde_json::from_str(stdout(&out).trim()).expect("json listing");
    let names: Vec<&str> = v["themes"]
        .as_array()
        .expect("themes array")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["dark"]);

    // And a name typed by hand is refused rather than silently changing
    // nothing, which is the harm the listing rule exists to prevent.
    for bad in ["blank", "titled", "stale"] {
        let out = cyclops(&home, &["theme", bad]);
        assert_eq!(out.status.code(), Some(2), "{bad}: {}", stdout(&out));
        assert!(
            stderr(&out).contains("sets no colors"),
            "{bad}: {}",
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("Nothing was changed"),
            "{bad}: {}",
            stderr(&out)
        );
        assert_eq!(config(&home), before, "{bad} edited the config");
    }
}

/// Scripts can do anything the UI does: the same listing as data.
#[test]
fn the_listing_has_a_json_form() {
    let home = scratch_home("theme-json");
    write_theme(&home, "dark", "#777777");
    write_theme(&home, "light", "#6e6e6e");
    let out = cyclops(&home, &["--json", "theme"]);
    let v: Value = serde_json::from_str(stdout(&out).trim()).expect("json listing");
    let names: Vec<&str> = v["themes"]
        .as_array()
        .expect("themes array")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["dark", "light"]);
    assert_eq!(v["themes"][0]["active"], json!(true));
    assert_eq!(v["themes"][1]["active"], json!(false));

    let out = cyclops(&home, &["--json", "theme", "light"]);
    let v: Value = serde_json::from_str(stdout(&out).trim()).expect("json switch");
    assert_eq!(v["theme"], json!("light"));
    assert_eq!(v["live"], json!(false));
    assert!(v["path"].as_str().expect("path").ends_with("light.toml"));
}
