//! cyclops: the thin CLI client for cyclopsd.
//!
//! Speaks NDJSON over the daemon's Unix socket (cyclops-proto) and renders
//! for humans. M0 surface: status, ping, read, watch. M1 adds send and the
//! hook receiver vendor hook configs invoke.

mod client;
mod copy;
mod hook;
mod render;
mod style;

use std::io::Write;
use std::time::Instant;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};

use client::Client;
use cyclops_proto::{
    DeliveryReceipt, DeliveryState, Event, MsgSendParams, MsgSendResult, PaneReadParams,
    PaneReadResult, PaneReadSource, StatusResult, SubscribeParams, PROTOCOL_VERSION,
};
use style::Style;

/// Usage mistakes exit 2 (clap's convention), keeping 1 to mean the
/// message ended parked or needing attention, which scripts branch on.
const EXIT_USAGE: i32 = 2;

#[derive(Parser)]
#[command(name = "cyclops", version, about = "One eye on every agent")]
struct Cli {
    /// Print raw results as JSON. Anything the UI shows, scripts can read.
    #[arg(long, global = true)]
    json: bool,

    /// No color, no glyph animation. Screen-reader friendly.
    #[arg(long, global = true)]
    plain: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// What cyclops is watching and the state of every agent.
    Status,
    /// Round-trip check against the daemon.
    Ping,
    /// Read a pane: visible screen, recent output, or the detection view.
    Read {
        /// Agent label or pane id, e.g. reviewer or %4.
        target: String,
        /// Cap the number of returned lines.
        #[arg(long)]
        lines: Option<u32>,
        #[arg(long, value_enum, default_value = "visible")]
        source: SourceArg,
    },
    /// Stream daemon events, one line each. Ctrl-C exits.
    Watch {
        /// Only these event kinds (prefix match), comma separated.
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
    },
    /// Send a message. The receipt names each delivery's state; exit 0 on
    /// delivered/queued, 1 on parked or needs attention.
    Send(SendArgs),
    /// Relay a vendor hook event to cyclops. Silent, always exits 0.
    Hook {
        /// Event name, e.g. Stop. An argument because agy payloads carry
        /// no event-name field (F7); the payload arrives on stdin.
        event: String,
        /// Reporting agent label; defaults to $CYCLOPS_AGENT.
        #[arg(long)]
        agent: Option<String>,
    },
}

#[derive(clap::Args)]
struct SendArgs {
    /// Recipient label or pane id, e.g. reviewer. Merges with --to.
    target: Option<String>,
    /// One line the recipient sees first.
    #[arg(long)]
    subject: String,
    /// Message body text.
    #[arg(long, conflicts_with = "body_file")]
    body: Option<String>,
    /// Read the body from a file; - reads stdin.
    #[arg(long)]
    body_file: Option<String>,
    /// More recipients, comma separated.
    #[arg(long, value_delimiter = ',')]
    to: Vec<String>,
    /// Every adopted agent.
    #[arg(long, conflicts_with_all = ["target", "to"])]
    all: bool,
    /// Announcement expecting no reply; the reply hint is dropped.
    #[arg(long)]
    fyi: bool,
    /// Message id this replies to, e.g. m-3f9c2a.
    #[arg(long)]
    reply_to: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum SourceArg {
    Visible,
    Recent,
    Detection,
}

impl From<SourceArg> for PaneReadSource {
    fn from(s: SourceArg) -> Self {
        match s {
            SourceArg::Visible => PaneReadSource::Visible,
            SourceArg::Recent => PaneReadSource::Recent,
            SourceArg::Detection => PaneReadSource::Detection,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let code = run(&cli);
    // process::exit skips destructors; make sure buffered output lands.
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn run(cli: &Cli) -> i32 {
    // Machine output never decorates, whatever the terminal supports.
    let style = if cli.json {
        Style::none()
    } else {
        Style::detect(cli.plain)
    };
    match &cli.cmd {
        // Hook never prints and owns its transport handling: a hook that
        // fails loudly breaks the vendor CLI that invoked it.
        Cmd::Hook { event, agent } => hook::run(event, agent.as_deref()),
        // Send validates usage and reads the body before touching the
        // daemon, so usage errors don't hide behind a down daemon.
        Cmd::Send(args) => cmd_send(cli, &style, args),
        Cmd::Status | Cmd::Ping | Cmd::Read { .. } | Cmd::Watch { .. } => {
            let mut c = match connect() {
                Ok(c) => c,
                Err(code) => return code,
            };
            match &cli.cmd {
                Cmd::Status => cmd_status(&mut c, cli, &style),
                Cmd::Ping => cmd_ping(&mut c, cli, &style),
                Cmd::Read {
                    target,
                    lines,
                    source,
                } => cmd_read(&mut c, cli, &style, target, *lines, (*source).into()),
                Cmd::Watch { kinds } => cmd_watch(&mut c, cli, &style, kinds),
                Cmd::Send(_) | Cmd::Hook { .. } => unreachable!("handled above"),
            }
        }
    }
}

/// Connect and check the hello. A protocol mismatch warns once on stderr
/// and continues: the protocol is tolerant by design (ADR-001, S2).
fn connect() -> Result<Client, i32> {
    match Client::connect() {
        Ok(c) => {
            let proto = c.hello().proto;
            if proto != PROTOCOL_VERSION {
                eprintln!("{}", copy::proto_mismatch(proto, PROTOCOL_VERSION));
            }
            Ok(c)
        }
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, None));
            Err(1)
        }
    }
}

fn cmd_status(c: &mut Client, cli: &Cli, style: &Style) -> i32 {
    let result = match c.request("status", json!({})) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, None));
            return 1;
        }
    };
    if cli.json {
        println!("{result}");
        return 0;
    }
    let status: StatusResult = match serde_json::from_value(result) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("{}", copy::UNREADABLE_ANSWER);
            return 1;
        }
    };
    let config = cyclops_proto::cyclops_home().join("config.toml");
    println!("{}", render::render_status(&status, style, &config));
    0
}

fn cmd_ping(c: &mut Client, cli: &Cli, style: &Style) -> i32 {
    let t0 = Instant::now();
    let result = c.request("ping", json!({}));
    let rtt_ms = t0.elapsed().as_secs_f64() * 1000.0;
    match result {
        Ok(v) => {
            if cli.json {
                let mut out = v;
                if let Value::Object(map) = &mut out {
                    map.insert("rtt_ms".into(), json!(rtt_ms));
                }
                println!("{out}");
            } else {
                println!("{}", render::render_ping(rtt_ms, style));
            }
            0
        }
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, None));
            1
        }
    }
}

fn cmd_read(
    c: &mut Client,
    cli: &Cli,
    style: &Style,
    target: &str,
    lines: Option<u32>,
    source: PaneReadSource,
) -> i32 {
    let params = serde_json::to_value(PaneReadParams {
        target: target.to_string(),
        source,
        lines,
    })
    .expect("pane.read params serialize");
    let result = match c.request("pane.read", params) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, Some(target)));
            return 1;
        }
    };
    if cli.json {
        println!("{result}");
        return 0;
    }
    let read: PaneReadResult = match serde_json::from_value(result) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("{}", copy::UNREADABLE_ANSWER);
            return 1;
        }
    };
    if let Some(det) = &read.detection {
        println!(
            "{}",
            render::render_detection(&read.target, det, style, render::now_ms())
        );
    } else if let Some(text) = &read.text {
        // Pane text verbatim, terminated by exactly one newline.
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    }
    0
}

fn cmd_send(cli: &Cli, style: &Style, args: &SendArgs) -> i32 {
    // Positional target merges into the to-list; --all is the whole list.
    let mut to: Vec<String> = Vec::new();
    if args.all {
        to.push("*".into());
    }
    for t in args.target.iter().chain(args.to.iter()) {
        if !to.contains(t) {
            to.push(t.clone());
        }
    }
    if to.is_empty() {
        eprintln!("{}", copy::NO_RECIPIENT);
        return EXIT_USAGE;
    }
    let body = match (&args.body, &args.body_file) {
        (Some(b), _) => b.clone(),
        (None, Some(path)) => match read_body_file(path) {
            Ok(b) => b,
            Err(cause) => {
                eprintln!("{}", copy::body_file_unreadable(path, &cause));
                return EXIT_USAGE;
            }
        },
        (None, None) => String::new(),
    };
    let mut c = match connect() {
        Ok(c) => c,
        Err(code) => return code,
    };
    let params = serde_json::to_value(MsgSendParams {
        to: to.clone(),
        subject: args.subject.clone(),
        body,
        fyi: args.fyi,
        reply_to: args.reply_to.clone(),
        wait: None,
    })
    .expect("msg.send params serialize");
    // With one recipient the unknown-target copy can name it; a broadcast
    // failure passes the daemon's copy through.
    let asked = if to.len() == 1 {
        Some(to[0].as_str())
    } else {
        None
    };
    let result = match c.request("msg.send", params) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, asked));
            return 1;
        }
    };
    if cli.json {
        println!("{result}");
        return receipts_exit_json(&result);
    }
    let receipt: MsgSendResult = match serde_json::from_value(result) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("{}", copy::UNREADABLE_ANSWER);
            return 1;
        }
    };
    println!("{}", render::render_receipts(&receipt.deliveries, style));
    // Parked recipients get the reset hint and next step spelled out.
    for d in &receipt.deliveries {
        if d.state == DeliveryState::ParkedBlockedQuota {
            eprintln!("{}", copy::parked(&d.to, d.note.as_deref()));
        }
    }
    receipts_exit(&receipt.deliveries)
}

/// Body from a file path, or stdin when the path is "-" (the v1 habit:
/// printf body | cyclops send ... --body-file -). Verbatim, no trimming:
/// the ledger records what was sent, not a cleaned-up version.
fn read_body_file(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).map_err(|e| e.to_string())?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }
}

/// Scripts branch on this: delivered, queued, and in-flight states exit 0;
/// parked and needs-attention exit 1.
fn receipts_exit(ds: &[DeliveryReceipt]) -> i32 {
    let bad = ds.iter().any(|d| {
        matches!(
            d.state,
            DeliveryState::ParkedBlockedQuota | DeliveryState::AttentionRequired
        )
    });
    i32::from(bad)
}

/// Same rule read tolerantly off the raw result for --json passthrough:
/// unknown states from a newer daemon don't break the exit code.
fn receipts_exit_json(v: &Value) -> i32 {
    let bad = v
        .get("deliveries")
        .and_then(Value::as_array)
        .is_some_and(|a| {
            a.iter().any(|d| {
                matches!(
                    d.get("state").and_then(Value::as_str),
                    Some("parked_blocked_quota" | "attention_required")
                )
            })
        });
    i32::from(bad)
}

fn cmd_watch(c: &mut Client, cli: &Cli, style: &Style, kinds: &[String]) -> i32 {
    let params = serde_json::to_value(SubscribeParams {
        kinds: kinds.to_vec(),
        cursor: None,
    })
    .expect("events.subscribe params serialize");
    if let Err(e) = c.request("events.subscribe", params) {
        eprintln!("{}", copy::client_error(&e, None));
        return 1;
    }
    // Streaming from here: no read deadline, block on the next event.
    c.clear_read_timeout();
    // Ctrl-C ends the process via default SIGINT handling. Every event is
    // written and flushed as a whole line, so an interrupt never leaves a
    // partial line behind.
    let mut stdout = std::io::stdout();
    loop {
        let line = match c.next_line() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{}", copy::client_error(&e, None));
                return 1;
            }
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("event").is_none() {
            continue;
        }
        if cli.json {
            let _ = writeln!(stdout, "{line}");
        } else if let Ok(ev) = serde_json::from_value::<Event>(v) {
            let _ = writeln!(
                stdout,
                "{}",
                render::render_event_line(&ev, style, render::now_ms())
            );
        }
        let _ = stdout.flush();
    }
}
