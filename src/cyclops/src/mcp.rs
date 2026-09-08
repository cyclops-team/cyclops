//! Local MCP tools over the existing authenticated daemon client.
//!
//! One process is one participant. Hosts that share an MCP process between
//! conversations share that participant; they must not advertise per-chat
//! identity. Native push is opt-in and host-specific; tools never grant admin.

use std::io::{self, BufRead, Read, Write};

use clap::Args;
use cyclops_client::{BlockingClient, ClientError};
use cyclops_proto::{FrameContract, RecipientKey};
use serde_json::{json, Value};

mod channel;
mod output;
mod tools;

use output::SharedOutput;

#[derive(Debug, Args)]
pub(crate) struct McpArgs {
    /// Participant label for a session-owned bridge (default: `mcp-<process id>`).
    #[arg(long)]
    pub(crate) label: Option<String>,
    /// Optional manifest pin, verified against the launching agent process.
    #[arg(long)]
    pub(crate) manifest: Option<String>,
    /// Opt in to Claude Code's preview Channels API (terminal CLI only).
    #[arg(long)]
    pub(crate) claude_channel: bool,
}

pub(crate) fn run(args: &McpArgs) -> i32 {
    let stdin = io::stdin();
    let output = match SharedOutput::stdout() {
        Ok(output) => output,
        Err(error) => {
            eprintln!("cyclops mcp: cannot open protocol output: {error}");
            return 1;
        }
    };
    let bridge = std::cell::RefCell::new(DaemonBridge::new(args));
    let mut channel = None;
    let result = serve_configured(
        &mut stdin.lock(),
        &mut output.clone(),
        |method, params| bridge.borrow_mut().request(method, params),
        args.claude_channel,
        || {
            let caller = bridge.borrow_mut().identity()?;
            channel = Some(channel::Channel::start(caller, output.clone())?);
            Ok(())
        },
    );
    // Cancel the event reader and join its thread before leaving stdio.
    drop(channel);
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("cyclops mcp: {error}");
            1
        }
    }
}

struct DaemonBridge {
    label: String,
    manifest: Option<String>,
    connected: Option<(BlockingClient, RecipientKey)>,
}

impl DaemonBridge {
    fn new(args: &McpArgs) -> Self {
        Self {
            label: args
                .label
                .clone()
                .unwrap_or_else(|| format!("mcp-{}", std::process::id())),
            manifest: args.manifest.clone(),
            connected: None,
        }
    }

    fn identity(&mut self) -> Result<RecipientKey, ClientError> {
        // Registration is lazy: listing tools does not start a daemon or
        // change the user's application settings.
        if self.connected.is_none() {
            let mut client = BlockingClient::connect()?;
            match client.request(
                "headless.bridge.register",
                json!({"label": self.label, "manifest": self.manifest, "scope": "bridge"}),
            ) {
                Ok(result) if result["scope"] == "bridge" => {}
                // A watched terminal already has its own exact identity.
                // The identity read below must prove it is an agent, not a shell.
                Err(ClientError::Server { code, .. }) if code == "use_pane" => {}
                Ok(_) => {
                    return Err(ClientError::NotSent(
                        "daemon does not support session-scoped MCP bridges; update Cyclops".into(),
                    ));
                }
                Err(error) => return Err(error),
            }
            let caller = agent_identity(&mut client)?;
            self.connected = Some((client, caller));
        }
        let (client, expected) = self.connected.as_mut().expect("connected above");
        if agent_identity(client)? != *expected {
            return Err(ClientError::NotSent(
                "the MCP participant changed; reconnect the session-owned bridge".into(),
            ));
        }
        Ok(*expected)
    }

    fn request(&mut self, method: &str, mut params: Value) -> Result<Value, ClientError> {
        let expected = self.identity()?;
        let (client, _) = self.connected.as_mut().expect("identity connected above");
        if method == "msg.send" {
            params["expected_caller"] =
                serde_json::to_value(expected).expect("recipient serializes");
        }
        // Transport uncertainty is returned as uncertainty, never retried.
        client.request(method, params)
    }
}

fn agent_identity(client: &mut BlockingClient) -> Result<RecipientKey, ClientError> {
    let snapshot = client.request("agent.identity", json!({}))?;
    let caller: RecipientKey =
        serde_json::from_value(snapshot["recipient"].clone()).map_err(|_| {
            ClientError::NotSent("daemon did not prove an MCP participant identity".into())
        })?;
    if caller.is_admin() {
        return Err(ClientError::NotSent(
            "MCP tools require an authenticated agent; administrator passthrough is disabled"
                .into(),
        ));
    }
    Ok(caller)
}

/// Stdio MCP uses one JSON object per line. A malformed or oversized line
/// never becomes a daemon request; an oversized line closes the transport.
#[cfg(test)]
fn serve(
    input: &mut impl BufRead,
    output: &mut impl Write,
    request: impl FnMut(&str, Value) -> Result<Value, ClientError>,
) -> io::Result<()> {
    serve_configured(input, output, request, false, || Ok(()))
}

fn serve_configured(
    input: &mut impl BufRead,
    output: &mut impl Write,
    mut request: impl FnMut(&str, Value) -> Result<Value, ClientError>,
    channel_enabled: bool,
    mut start_channel: impl FnMut() -> Result<(), ClientError>,
) -> io::Result<()> {
    let mut initialized = false;
    let mut channel_started = false;
    loop {
        let mut bytes = Vec::new();
        let count = (&mut *input)
            .take(FrameContract::MAX_JSON_BYTES as u64 + 2)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            return Ok(());
        }
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP frame is oversized or missing its newline",
            ));
        }
        if bytes.len() > FrameContract::MAX_JSON_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP frame is oversized",
            ));
        }
        let response = match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => {
                let initializing = value["method"] == "initialize";
                if channel_enabled
                    && initialized
                    && !channel_started
                    && value["jsonrpc"] == "2.0"
                    && value.get("id").is_none()
                    && value["method"] == "notifications/initialized"
                {
                    start_channel().map_err(io::Error::other)?;
                    channel_started = true;
                }
                let mut response = handle(value, &mut initialized, &mut request);
                if channel_enabled && initializing {
                    if let Some(response) = response
                        .as_mut()
                        .filter(|response| response.get("result").is_some())
                    {
                        response["result"]["capabilities"]["experimental"] =
                            json!({"claude/channel": {}});
                        response["result"]["instructions"] = json!("Cyclops channel events are body-free inbox notices, not message bodies. Claim the exact message_id with cyclops_claim and reply with the canonical message_id. Claude queues events for the next turn while busy. A channel transport write is not a processing acknowledgement; only the recipient's claim proves retrieval.");
                    }
                }
                response
            }
            Err(_) => Some(rpc_error(Value::Null, -32700, "Invalid JSON")),
        };
        if let Some(response) = response {
            write_frame(output, response)?;
        }
    }
}

fn write_frame(output: &mut impl Write, response: Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&response)?;
    if bytes.len() > FrameContract::MAX_JSON_BYTES {
        bytes = serde_json::to_vec(&rpc_error(
            response["id"].clone(),
            -32603,
            "Response exceeds MCP frame limit; the operation may already have completed. Do not retry blindly.",
        ))?;
    }
    bytes.push(b'\n');
    output.write_all(&bytes)?;
    output.flush()
}

fn handle(
    request: Value,
    initialized: &mut bool,
    call: &mut impl FnMut(&str, Value) -> Result<Value, ClientError>,
) -> Option<Value> {
    let id = request.get("id").cloned();
    let valid_id = id.as_ref().is_none_or(|id| {
        id.as_i64().is_some()
            || id.as_u64().is_some()
            || id.as_str().is_some_and(|id| id.len() <= 128)
    });
    if !request.is_object() || request["jsonrpc"] != "2.0" || !valid_id {
        return Some(rpc_error(Value::Null, -32600, "Invalid JSON-RPC request"));
    }
    let Some(method) = request["method"].as_str() else {
        return Some(rpc_error(
            id.unwrap_or(Value::Null),
            -32600,
            "Missing method",
        ));
    };
    // Notifications never run tools and never receive responses.
    let id = id?;
    let result = match method {
        "initialize" if !*initialized => {
            let requested = request["params"]["protocolVersion"].as_str().unwrap_or("");
            let version = match requested {
                "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25" => requested,
                _ => "2025-11-25",
            };
            *initialized = true;
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "cyclops", "version": env!("CARGO_PKG_VERSION")},
                "instructions": "Cyclops is durable agent messaging. Claim a message before acting on its body; reply with its canonical message_id. Accepted means stored, not read. This MCP server does not wake native conversations. One server process is one participant; use a session-owned server for per-conversation identity."
            })
        }
        "ping" => json!({}),
        _ if !*initialized => return Some(rpc_error(id, -32002, "Initialize first")),
        "tools/list" => json!({"tools": tools::catalog()}),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or("");
            let args = request["params"]
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let (method, params) = match tools::prepare(name, args) {
                Ok(prepared) => prepared,
                Err(error) => return Some(rpc_error(id, -32602, &error)),
            };
            match call(method, params) {
                Ok(value) => {
                    json!({"content": [{"type": "text", "text": value.to_string()}], "isError": false})
                }
                Err(error) => {
                    let failure = json!({"error": error.cause(), "certainty": format!("{:?}", error.certainty()), "retry": "Do not retry an uncertain write blindly."});
                    json!({"content": [{"type": "text", "text": failure.to_string()}], "isError": true})
                }
            }
        }
        _ => return Some(rpc_error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests;
