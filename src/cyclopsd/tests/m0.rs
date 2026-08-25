//! M0 end-to-end: a real tmux server on an isolated `-L` socket, the
//! daemon booted in-process, an NDJSON client on the Unix socket.
//!
//! Never touches the user's tmux: every tmux call carries
//! `-L cyc-m0-<pid> -f /dev/null` through cyclops-testrig, which kills
//! the server and unlinks its socket on drop.
//! Skips cleanly when tmux is not on PATH.
//!
//! One test function on purpose: the scenario is sequential (boot, ping,
//! status, subscribe, retitle, read) and a single flow avoids CYCLOPS_HOME
//! and tmux-server races between parallel tests. The bounded poll loops in
//! here are test-side waits, explicitly outside the daemon's zero-polling
//! contract.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cyclops_testrig::{tmux_available, TmuxServer};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

/// Fixture manifest: binds one test-owned shell alias and gives fusion one title
/// tier and one screen tier with deliberately different states, so both
/// the title-decides path and the disagreement path are exercised without
/// any vendor CLI.
const FIXTURE_MANIFEST: &str = r#"
[agent]
id = "fixture"
display_name = "M0 fixture"
process_names = ["cyc-m0-agent"]

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^IDLE']

[[rule]]
id = "screen_busy"
state = "working"
priority = 800
region = "bottom_non_empty_lines(3)"
line_regex = ['^FIXPROMPT']
"#;

/// Scratch CYCLOPS_HOME under the relocatable scratch root (F24), removed
/// on drop.
struct HomeGuard(PathBuf);

impl Drop for HomeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Minimal NDJSON test client. Events that arrive while waiting for a
/// response are buffered for next_event.
struct TestClient {
    lines: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    pending_events: VecDeque<Value>,
    next_id: u64,
}

impl TestClient {
    async fn connect(path: &Path) -> (TestClient, Value) {
        let stream = UnixStream::connect(path).await.expect("connect daemon");
        let (r, writer) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        let hello_line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("hello within 5s")
            .expect("read hello")
            .expect("hello line");
        let hello: Value = serde_json::from_str(&hello_line).expect("hello parses");
        (
            TestClient {
                lines,
                writer,
                pending_events: VecDeque::new(),
                next_id: 1,
            },
            hello,
        )
    }

    async fn send_raw(&mut self, text: &str) {
        self.writer
            .write_all(text.as_bytes())
            .await
            .expect("write to daemon");
    }

    async fn next_line(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(5), self.lines.next_line())
            .await
            .expect("line within 5s")
            .expect("read line")
            .expect("connection open");
        serde_json::from_str(&line).expect("line parses")
    }

    /// Next non-event line (a response), buffering events on the way.
    async fn next_response(&mut self) -> Value {
        loop {
            let v = self.next_line().await;
            if v.get("event").is_some() {
                self.pending_events.push_back(v);
                continue;
            }
            return v;
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        self.send_raw(&format!("{line}\n")).await;
        loop {
            let v = self.next_response().await;
            if v["id"] == json!(id) {
                return v;
            }
        }
    }

    async fn next_event(&mut self, within: Duration) -> Value {
        if let Some(e) = self.pending_events.pop_front() {
            return e;
        }
        let deadline = Instant::now() + within;
        loop {
            assert!(Instant::now() < deadline, "no event within {within:?}");
            let v = self.next_line().await;
            if v.get("event").is_some() {
                return v;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn m0_shadow_daemon_end_to_end() {
    if !tmux_available() {
        eprintln!("skipping m0 integration test: tmux not on PATH");
        return;
    }
    // Isolated tmux server with one bash pane. Created before the config
    // that names it, so its teardown is armed for the whole test.
    let tmux = TmuxServer::new("m0");
    let tmux_socket = tmux.socket();
    let home = cyclops_proto::scratch::scratch_dir("cyc-m0-home");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("manifests")).expect("create scratch home");
    let _home_guard = HomeGuard(home.clone());
    std::fs::write(home.join("manifests/fixture.toml"), FIXTURE_MANIFEST).expect("write manifest");
    let fixture_bin = cyclops_proto::scratch::scratch_dir("cyc-m0-bin");
    let _ = std::fs::remove_dir_all(&fixture_bin);
    std::fs::create_dir_all(&fixture_bin).expect("create fixture bin");
    let _bin_guard = HomeGuard(fixture_bin.clone());
    let fixture_shell = fixture_bin.join("cyc-m0-agent");
    std::os::unix::fs::symlink("/bin/bash", &fixture_shell).expect("link fixture shell");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "sessions = [\"main\"]\n\
             tmux_socket = \"{tmux_socket}\"\n\
             tmux_config = \"/dev/null\"\n\
             manifest_dir = \"{}\"\n\
             future_key = true\n",
            home.join("manifests").display()
        ),
    )
    .expect("write config");

    // --norc --noprofile keeps the user's shell config out of the fixture.
    tmux.run_ok(&[
        "new-session",
        "-d",
        "-s",
        "main",
        "-x",
        "100",
        "-y",
        "30",
        &format!("{} --norc --noprofile", fixture_shell.display()),
    ]);
    tmux.run_ok(&["send-keys", "-t", "main", "PS1='FIXPROMPT '", "Enter"]);
    // Wait until the fixture prompt renders (screen tier signal).
    let t_prompt = Instant::now();
    while !tmux
        .capture("main")
        .lines()
        .any(|l| l.starts_with("FIXPROMPT"))
    {
        assert!(
            t_prompt.elapsed() < Duration::from_secs(5),
            "fixture prompt never rendered: {}",
            tmux.capture("main")
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Config load exercises the unknown-key warning path.
    let (cfg, warnings) = cyclopsd::Config::load(&home).expect("config loads");
    assert!(
        warnings.iter().any(|w| w.contains("future_key")),
        "unknown key should warn: {warnings:?}"
    );
    assert_eq!(cfg.sessions, vec!["main"]);

    let t_boot = Instant::now();
    let daemon = cyclopsd::boot(cfg).await.expect("daemon boots");
    let sock = daemon.socket_path();
    assert!(sock.exists(), "socket bound at {}", sock.display());

    // Hello and ping.
    let (mut c, hello) = TestClient::connect(&sock).await;
    assert_eq!(hello["proto"], 1);
    assert_eq!(hello["build"], env!("CYCLOPS_BUILD_REF"));
    assert!(!hello["boot_id"].as_str().unwrap().is_empty());
    let resp = c.request("ping", json!({})).await;
    assert_eq!(resp["result"]["pong"], true);

    // Malformed JSON: bad_request with null id, connection stays open.
    c.send_raw("this is not json\n").await;
    let bad = c.next_response().await;
    assert_eq!(bad["error"]["code"], "bad_request");
    assert!(bad["id"].is_null());
    let resp = c.request("ping", json!({})).await;
    assert_eq!(resp["result"]["pong"], true);

    // msg.send and agent.state.report are implemented since M1; empty
    // params are a parse error. msg.history is implemented since M2 and
    // answers an unfiltered query with the (empty so far) record.
    let resp = c.request("msg.send", json!({})).await;
    assert_eq!(resp["error"]["code"], "bad_request");
    let resp = c.request("agent.state.report", json!({})).await;
    assert_eq!(resp["error"]["code"], "bad_request");
    let resp = c.request("msg.history", json!({})).await;
    assert!(resp["error"].is_null(), "{resp}");
    assert_eq!(resp["result"]["lines"], json!([]));

    // Status: attached, one bash pane bound to the fixture manifest, state
    // working (default title matches nothing; only the screen tier fires).
    let deadline = Instant::now() + Duration::from_secs(10);
    let pane_id = loop {
        let resp = c.request("status", json!({})).await;
        assert_eq!(resp["result"]["daemon_build"], env!("CYCLOPS_BUILD_REF"));
        let session = &resp["result"]["sessions"][0];
        if session["attached"] == json!(true) {
            let pane = &session["panes"][0];
            // tmux reports either the symlink basename or its shell target,
            // depending on the host platform.
            if matches!(
                pane["current_command"].as_str(),
                Some("bash") | Some("cyc-m0-agent")
            ) && pane["manifest"] == "fixture"
                && pane["state"] == "working"
            {
                assert_eq!(session["name"], "main");
                assert_eq!(pane["in_mode"], false);
                break pane["pane_id"].as_str().unwrap().to_string();
            }
        }
        assert!(
            Instant::now() < deadline,
            "status never showed the fixture pane as working: {resp}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let ready_ms = t_boot.elapsed();
    eprintln!("boot to attached and fused status: {ready_ms:?}");
    // GOALS: startup feels under 300 ms once tmux is up. The assert leaves
    // slack for a loaded CI box while still catching a pathological boot.
    assert!(
        ready_ms < Duration::from_millis(1500),
        "startup took {ready_ms:?}"
    );

    // Subscribe on a second connection, then flip the title rule.
    let (mut watcher_client, _) = TestClient::connect(&sock).await;
    let ack = watcher_client
        .request("events.subscribe", json!({"kinds": ["state"]}))
        .await;
    assert_eq!(ack["result"]["subscribed"], true);
    tmux.run_ok(&["select-pane", "-t", "main", "-T", "IDLE ready"]);
    let ev = watcher_client.next_event(Duration::from_secs(5)).await;
    assert_eq!(ev["event"], "state");
    assert_eq!(ev["data"]["pane_id"].as_str(), Some(pane_id.as_str()));
    assert_eq!(ev["data"]["state"], "idle");
    assert_eq!(ev["data"]["prior"], "working");

    // pane.read visible sees a marker printed in the pane.
    tmux.run_ok(&["send-keys", "-t", "main", "echo cyclops-M0-marker", "Enter"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let resp = c
            .request("pane.read", json!({"target": pane_id, "source": "visible"}))
            .await;
        let text = resp["result"]["text"].as_str().unwrap_or_default();
        if text.lines().any(|l| l.trim() == "cyclops-M0-marker") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "marker never appeared in pane.read visible: {resp}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // pane.read detection forces the full sensor set: title says idle,
    // screen says working, verdict goes to the higher priority rule and
    // the disagreement is observable.
    let resp = c
        .request(
            "pane.read",
            json!({"target": pane_id, "source": "detection"}),
        )
        .await;
    let det = &resp["result"]["detection"];
    assert_eq!(det["state"], "idle");
    assert_eq!(det["decided_by"], "title_idle");
    assert_eq!(det["disagreement"], true);
    let readings = det["readings"].as_array().expect("readings array");
    assert_eq!(readings.len(), 2, "{readings:?}");
    assert_eq!(readings[0]["sensor"], "title");
    assert_eq!(readings[0]["rule"], "title_idle");
    assert_eq!(readings[1]["sensor"], "screen");
    assert_eq!(readings[1]["rule"], "screen_busy");

    // pane.read resolves adoption labels too, like every verb that
    // promises "label or pane id" (the v1 shim maps `read <label>` here).
    let resp = c
        .request("pane.label", json!({"target": pane_id, "label": "fixture"}))
        .await;
    assert!(resp["error"].is_null(), "{resp}");
    let resp = c
        .request(
            "pane.read",
            json!({"target": "fixture", "source": "visible"}),
        )
        .await;
    assert!(resp["error"].is_null(), "{resp}");
    let text = resp["result"]["text"].as_str().unwrap_or_default();
    assert!(
        text.lines().any(|l| l.trim() == "cyclops-M0-marker"),
        "label-resolved read missed the pane text: {resp}"
    );

    // Unknown target answers no_such_target and names known panes.
    let resp = c.request("pane.read", json!({"target": "ghost"})).await;
    assert_eq!(resp["error"]["code"], "no_such_target");
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&pane_id),
        "{resp}"
    );

    // Clean shutdown removes the socket.
    daemon.shutdown().await;
    assert!(!sock.exists(), "socket removed on shutdown");
}
