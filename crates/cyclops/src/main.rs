//! cyclops: the thin CLI client for cyclopsd.
//!
//! Speaks NDJSON over the daemon's Unix socket (cyclops-proto) and renders
//! for humans. M0 surface: status, ping, read, watch. Messaging verbs land
//! in M1 and are deliberately absent here.

mod client;
mod copy;
mod render;
mod style;

use std::io::Write;
use std::time::Instant;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};

use client::Client;
use cyclops_proto::{
    Event, PaneReadParams, PaneReadResult, PaneReadSource, StatusResult, SubscribeParams,
    PROTOCOL_VERSION,
};
use style::Style;

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
