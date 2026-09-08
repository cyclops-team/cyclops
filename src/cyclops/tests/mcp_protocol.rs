//! The actual CLI must not decorate MCP stdout or start/setup a daemon.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

#[test]
fn mcp_handshake_is_protocol_only_and_does_not_create_a_state_home() {
    let home = cyclops_proto::scratch::scratch_dir("cyc-mcp-protocol-no-home");
    assert!(!home.exists(), "this test never creates its state home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_cyclops"))
        .arg("mcp")
        .env("CYCLOPS_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25"}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ] {
        writeln!(input, "{message}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let replies: Vec<Value> = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 5);
    assert!(
        !home.exists(),
        "listing tools must not setup or start Cyclops"
    );
}
