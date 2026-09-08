//! The complete MCP authority surface. No arbitrary daemon method or
//! caller identity passes through this adapter.

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    to: Vec<String>,
    subject: String,
    #[serde(default)]
    summary: Option<String>,
    body: String,
    #[serde(default)]
    client_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    message_id: String,
    #[serde(default)]
    summary: Option<String>,
    body: String,
    #[serde(default)]
    client_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inbox {
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    message_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Thread {
    id: String,
    #[serde(default)]
    body: bool,
}

fn decode<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|error| error.to_string())
}

pub(super) fn prepare(name: &str, args: Value) -> Result<(&'static str, Value), String> {
    Ok(match name {
        "cyclops_send" => {
            let args: Send = decode(args)?;
            (
                "msg.send",
                json!({"to": args.to, "subject": args.subject, "summary": args.summary, "body": args.body, "client_key": args.client_key}),
            )
        }
        "cyclops_reply" => {
            let args: Reply = decode(args)?;
            (
                "msg.reply",
                json!({"message_id": args.message_id, "summary": args.summary, "body": args.body, "client_key": args.client_key}),
            )
        }
        "cyclops_inbox" => {
            let args: Inbox = decode(args)?;
            ("inbox.list", json!({"limit": args.limit}))
        }
        "cyclops_claim" => {
            let args: Claim = decode(args)?;
            ("inbox.claim", json!({"message_id": args.message_id}))
        }
        "cyclops_thread" => {
            let args: Thread = decode(args)?;
            ("msg.thread", json!({"id": args.id, "body": args.body}))
        }
        _ => return Err(format!("Unknown Cyclops tool: {name}")),
    })
}

pub(super) fn catalog() -> Value {
    let text = || json!({"type": "string"});
    json!([
        tool("cyclops_send", "Store a message for named Cyclops participants. Returns durable acceptance and notification state, not proof of reading. Summary is optional; the daemon derives the preview when omitted.", json!({"to": {"type": "array", "items": text()}, "subject": text(), "summary": text(), "body": text(), "client_key": text()}), &["to", "subject", "body"], false),
        tool("cyclops_reply", "Reply to a canonical message_id obtained through your own claim. Routing, sender identity, and the optional summary fallback are daemon-owned.", json!({"message_id": text(), "summary": text(), "body": text(), "client_key": text()}), &["message_id", "body"], false),
        tool("cyclops_inbox", "List your pending message summaries without claiming or reading bodies.", json!({"limit": {"type": "integer", "minimum": 0, "maximum": 4294967295u64}}), &[], true),
        tool("cyclops_claim", "Claim one exact canonical message_id or m-att_ locator addressed to you; returns the authoritative full message.", json!({"message_id": text()}), &["message_id"], false),
        tool("cyclops_thread", "Read an authorized message thread; bodies remain hidden unless you have claimed them.", json!({"id": text(), "body": {"type": "boolean"}}), &["id"], true)
    ])
}

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required, "additionalProperties": false},
        "annotations": {"readOnlyHint": read_only, "destructiveHint": false, "openWorldHint": false}
    })
}
