//! Optional daemon queries for confirmation flows.

use std::path::Path;

use cyclops_proto::{socket_path, PaneStatus, StatusResult, PROTOCOL_VERSION};

/// True when the daemon reports an adopted agent in this pane.
pub fn pane_has_agent(_home: &Path, pane_id: &str) -> bool {
    let sock = socket_path();
    if !sock.exists() {
        return false;
    }
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) else {
        return false;
    };
    use std::io::Write;
    let req = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"status\",\"params\":{{\"protocol_version\":{PROTOCOL_VERSION}}}}}\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    use std::io::BufRead;
    let mut line = String::new();
    let mut reader = std::io::BufReader::new(stream);
    if reader.read_line(&mut line).is_err() {
        return false;
    }
    let Ok(resp) = serde_json::from_str::<serde_json::Value>(&line) else {
        return false;
    };
    let Some(result) = resp.get("result") else {
        return false;
    };
    let Ok(status) = serde_json::from_value::<StatusResult>(result.clone()) else {
        return false;
    };
    status
        .sessions
        .iter()
        .flat_map(|s| s.panes.iter())
        .any(|p: &PaneStatus| p.pane_id == pane_id && p.agent.is_some())
}
