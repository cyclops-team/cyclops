//! cyclopsd entry point: logging, config, signals. Everything else lives
//! in the library so integration tests can boot the daemon in-process.

use std::process::ExitCode;

use clap::Parser;
use tracing::{error, info, warn};

use cyclopsd::Config;

#[derive(Parser)]
#[command(
    name = "cyclopsd",
    version,
    about = "The Cyclops daemon for pane state, durable mailboxes, and safe notifications"
)]
struct Cli {}

#[tokio::main]
async fn main() -> ExitCode {
    Cli::parse();
    // CYCLOPS_LOG uses EnvFilter syntax; default info. Human output on
    // stderr, stdout stays free for nothing (the daemon has no stdout).
    let filter = tracing_subscriber::EnvFilter::try_from_env("CYCLOPS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let home = cyclops_proto::cyclops_home();
    let (cfg, warnings) = match Config::load(&home) {
        Ok(v) => v,
        Err(e) => {
            error!("config: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    for w in &warnings {
        warn!("config: {w}");
    }
    if cfg.sessions.is_empty() {
        info!("no sessions configured; watching nothing, status still answers");
    }

    let daemon = match cyclopsd::boot(cfg).await {
        Ok(d) => d,
        Err(e) => {
            error!("boot failed: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    info!(socket = %daemon.socket_path().display(), "cyclopsd ready");

    wait_for_signal().await;
    daemon.shutdown().await;
    ExitCode::SUCCESS
}

/// Block until SIGINT or SIGTERM.
async fn wait_for_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("SIGINT received, shutting down"),
        _ = term.recv() => info!("SIGTERM received, shutting down"),
    }
}
