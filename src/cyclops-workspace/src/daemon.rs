//! Small, bounded requests to cyclopsd for workspace decoration and naming.
//!
//! The daemon speaks Hello-first NDJSON. Every helper in this module consumes
//! that Hello before sending its request; keeping the transport here prevents
//! confirmation, decoration, and naming from drifting into subtly different
//! protocol clients.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use cyclops_proto::{Hello, PaneStatus, Request, Response, StatusParams, StatusResult, SOCK_NAME};
use serde_json::{json, Value};

const IO_TIMEOUT: Duration = Duration::from_millis(250);

fn connect(home: &Path) -> Result<BufReader<UnixStream>, String> {
    let path = home.join(SOCK_NAME);
    let stream = UnixStream::connect(&path)
        .map_err(|error| format!("cyclopsd is unavailable at {}: {error}", path.display()))?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("cannot set cyclopsd read deadline: {error}"))?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|error| format!("cannot set cyclopsd write deadline: {error}"))?;
    let mut reader = BufReader::new(stream);

    let hello = read_value(&mut reader, "hello")?;
    serde_json::from_value::<Hello>(hello)
        .map_err(|error| format!("cyclopsd sent an unreadable hello: {error}"))?;
    Ok(reader)
}

fn write_request(stream: &mut UnixStream, method: &str, params: Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(&Request {
        id: json!(1),
        method: method.to_string(),
        params,
    })
    .map_err(|error| format!("cannot encode {method} request: {error}"))?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|error| format!("cannot write {method} request: {error}"))
}

/// One request on a fresh connection. Requests are infrequent user actions
/// or event-triggered refreshes, so a short-lived connection is simpler and
/// safer than sharing the daemon subscription's stream.
pub(crate) fn request(home: &Path, method: &str, params: Value) -> Result<Value, String> {
    exchange(&mut connect(home)?, method, params)
}

/// The request/response half of [`request`], on an already-open
/// connection. Split out because [`theme_reload`] has to tell "nothing
/// answered on the socket" apart from "a daemon answered and refused".
fn exchange(
    reader: &mut BufReader<UnixStream>,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    write_request(reader.get_mut(), method, params)?;

    // A fresh, unsubscribed connection has exactly one response after its
    // Hello. Anything else is a protocol error; failing immediately keeps a
    // malformed peer from extending this bounded request indefinitely.
    let response = read_value(reader, method)?;
    let response = serde_json::from_value::<Response>(response)
        .map_err(|error| format!("cyclopsd sent an unreadable {method} response: {error}"))?;
    if response.id != json!(1) {
        return Err(format!(
            "cyclopsd replied to {method} with the wrong request id"
        ));
    }
    if let Some(error) = response.error {
        return Err(error.message);
    }
    response
        .result
        .ok_or_else(|| format!("cyclopsd omitted the {method} result"))
}

fn read_value(reader: &mut BufReader<UnixStream>, context: &str) -> Result<Value, String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return Err(format!("cyclopsd closed during {context}")),
        Ok(_) => {}
        Err(error) => return Err(format!("cannot read cyclopsd {context}: {error}")),
    }
    serde_json::from_str(line.trim())
        .map_err(|error| format!("cyclopsd sent unreadable {context}: {error}"))
}

/// Current daemon status. Callers pass the shared protocol params so the
/// choice to include the delivery half of attention stays visible at the
/// surface that needs it.
pub fn status(home: &Path, params: StatusParams) -> Result<StatusResult, String> {
    let params = serde_json::to_value(params)
        .map_err(|error| format!("cannot encode cyclopsd status request: {error}"))?;
    let value = request(home, "status", params)?;
    serde_json::from_value(value).map_err(|error| format!("unreadable cyclopsd status: {error}"))
}

/// True when the daemon reports an adopted agent in this pane.
pub fn pane_has_agent(home: &Path, pane_id: &str) -> bool {
    status(home, StatusParams::default())
        .map(|status| {
            status
                .sessions
                .iter()
                .flat_map(|session| session.panes.iter())
                .any(|pane: &PaneStatus| pane.pane_id == pane_id && pane.agent.is_some())
        })
        .unwrap_or(false)
}

/// Ask cyclopsd to watch a session it was not booted with.
///
/// `sessions` in config.toml is the daemon's BOOT set, and a workspace this
/// UI creates is never in it. An unwatched session has no pane table at all,
/// so nothing in it can be detected, named, or given a state: its sidebar
/// row would stay empty for as long as the daemon runs.
///
/// Callers ask once per session id and never again. A tmux control
/// connection survives a session rename — the daemon holds the session
/// object, not its name — so re-asking under the new name would build a
/// second slot naming something nothing answers to, and leave its watcher
/// retrying an attach for the daemon's whole life.
pub fn watch_session(home: &Path, session: &str) -> Result<(), String> {
    request(home, "session.watch", json!({"session": session}))?;
    Ok(())
}

/// How far a theme nudge got: the two answers a caller has to tell apart
/// because they are two different stories (the CLI's `Switch`).
#[derive(Debug, PartialEq, Eq)]
pub enum ThemeReload {
    /// A daemon answered; carries the theme it says it is NOW painting,
    /// which is not always the one just chosen (a `CYCLOPS_THEME` pinned
    /// in its environment beats the config key). `None` when the answer
    /// named no theme, or the daemon refused: either way nothing on
    /// screen is confirmed to have moved.
    Painting(Option<String>),
    /// Nothing answered on the socket. There is no screen to be wrong
    /// about; the next command reads the key.
    NoDaemon,
}

/// Tell a running cyclopsd the theme key moved: the same `theme.reload`
/// nudge `cyclops theme <name>` sends. Takes no theme name on purpose,
/// the daemon re-resolves the selection itself.
pub fn theme_reload(home: &Path) -> ThemeReload {
    let Ok(mut reader) = connect(home) else {
        return ThemeReload::NoDaemon;
    };
    match exchange(&mut reader, "theme.reload", json!({})) {
        Ok(result) => ThemeReload::Painting(
            result
                .get("theme")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        Err(_) => ThemeReload::Painting(None),
    }
}

/// Assign the pane's Cyclops identity. Detection remains the daemon's job;
/// omitting `manifest` preserves the CLI's normal auto-detection behavior.
pub fn label_pane(home: &Path, pane_id: &str, label: &str) -> Result<(), String> {
    if let Some(why) = cyclops_proto::label::refusal(label) {
        return Err(why);
    }
    request(
        home,
        "pane.label",
        json!({"target": pane_id, "label": label, "manifest": Value::Null}),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn request_consumes_the_mandatory_hello_before_the_response() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-daemon-hello");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let socket = home.join(SOCK_NAME);
        let listener = UnixListener::bind(&socket).expect("listen");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut request = String::new();
            reader.read_line(&mut request).expect("request");
            assert_eq!(
                serde_json::from_str::<Value>(&request).unwrap()["method"],
                "ping"
            );
            reader
                .get_mut()
                .write_all(b"{\"id\":1,\"result\":{\"pong\":true}}\n")
                .expect("response");
        });

        let response = request(&home, "ping", json!({})).expect("request succeeds");
        assert_eq!(response["pong"], true);
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn theme_reload_reads_what_the_daemon_says_it_is_painting() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-theme-reload");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let listener = UnixListener::bind(home.join(SOCK_NAME)).expect("listen");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut line = String::new();
            reader.read_line(&mut line).expect("request");
            let request: Value = serde_json::from_str(&line).expect("JSON request");
            assert_eq!(request["method"], "theme.reload");
            reader
                .get_mut()
                .write_all(b"{\"id\":1,\"result\":{\"theme\":\"solar\"}}\n")
                .expect("response");
        });

        assert_eq!(
            theme_reload(&home),
            ThemeReload::Painting(Some("solar".into()))
        );
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn theme_reload_with_nothing_on_the_socket_is_no_daemon() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-theme-reload-down");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        assert_eq!(theme_reload(&home), ThemeReload::NoDaemon);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn pane_labels_fail_before_io_when_the_name_is_reserved() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-bad-label");
        let error = label_pane(&home, "%1", "admin").expect_err("reserved label");
        assert!(error.contains("you"), "{error}");
    }

    #[test]
    fn pane_label_uses_the_daemons_addressing_method() {
        let home = cyclops_proto::scratch::scratch_dir("workspace-pane-label");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let listener = UnixListener::bind(home.join(SOCK_NAME)).expect("listen");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            reader
                .get_mut()
                .write_all(b"{\"cyclops\":\"0.1.0\",\"proto\":1,\"boot_id\":\"b\"}\n")
                .expect("hello");
            let mut line = String::new();
            reader.read_line(&mut line).expect("request");
            let request: Value = serde_json::from_str(&line).expect("JSON request");
            assert_eq!(request["method"], "pane.label");
            assert_eq!(request["params"]["target"], "%7");
            assert_eq!(request["params"]["label"], "reviewer");
            assert!(request["params"]["manifest"].is_null());
            reader
                .get_mut()
                .write_all(b"{\"id\":1,\"result\":{\"labeled\":true}}\n")
                .expect("response");
        });

        label_pane(&home, "%7", "reviewer").expect("label succeeds");
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(home);
    }
}
