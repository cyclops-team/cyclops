//! cyclops: the thin CLI client for cyclopsd.
//!
//! ## What it owns
//!
//! Two jobs, and nothing between them. It speaks NDJSON over the daemon's
//! Unix socket (`client.rs`, types from `cyclops-proto`), and it renders
//! what comes back for a human (`render.rs` layout, `style.rs` color,
//! `copy.rs` words). Every verb takes `--json` and then prints exactly the
//! socket answer, which is the promise the rendering exists to be optional
//! against. `cyclops ui` is the one verb with no `--json`; the machine
//! stream is `cyclops watch --json`.
//!
//! Three things here are not that shape and say why:
//!
//! - `hook.rs`, the receiver vendor hook configs invoke. It runs inside a
//!   vendor's hook budget, so it is fast, silent, and exits 0 regardless.
//! - `hookset.rs`, which renders vendor hook configs and never writes into
//!   a vendor dot-dir.
//! - `workspace.rs`, the only place this binary reaches tmux, and it does
//!   that through `cyclops_tmux::layout`. What is left here is files, the
//!   config keys, the daemon round trips, and the copy.
//!
//! ## What it does not own
//!
//! - Any decision. A verb asks the daemon and prints the answer; it does
//!   not compute state, and it does not judge what needs a human (that is
//!   `cyclops_proto::attention`, which `render.rs` reads).
//! - The stream. `cyclops ui` dispatches into `cyclops-ui`.
//! - The voice of a state cell, a badge, or the clock gutter. Those are
//!   `cyclops_ui::grid`, which this crate calls rather than copies. A copy
//!   lived here once and drifted.
//! - Any color value. Every paint is a `cyclops-theme` token.
//!
//! Verbs, by the milestone that added them: `ping`, `status`, `read`,
//! `watch` (M0); `send`, `hook` (M1); `history`, `thread`, `wait`,
//! `send --wait`, `hooks install|verify|selftest` (M2); `ui` (M3); `name`,
//! `list`, `start`, `workspace save|restore` (M4); `theme` (M5);
//! `update` (post-M5).

mod client;
mod copy;
mod daemon;
mod hash;
mod hook;
mod hookset;
mod manifests;
mod render;
mod skillseed;
mod style;
mod theme;
mod themeseed;
mod update;
mod workspace;

use std::io::IsTerminal;
use std::io::Write;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};

use client::{Client, ClientError};
use cyclops_proto::{
    delivery_needs_human, DeliveryReceipt, DeliveryState, Event, HistoryParams, HistoryResult,
    MsgSendParams, MsgSendResult, PaneReadParams, PaneReadResult, PaneReadSource, PaneStatus,
    StatusResult, SubscribeParams, ThreadResult, WaitSpec, WaitUntil, PROTOCOL_VERSION,
};
use style::Style;

/// Usage mistakes exit 2 (clap's convention), keeping 1 to mean the
/// message ended parked or needing attention, which scripts branch on.
const EXIT_USAGE: i32 = 2;

/// `cyclops wait` exit codes scripts branch on: 2 the timeout expired, 3
/// the pinned pane died or changed occupant mid-wait. Reached exits 0;
/// transport and unknown-target errors keep the usual 1.
const EXIT_WAIT_TIMEOUT: i32 = 2;
const EXIT_OCCUPANT_CHANGED: i32 = 3;

/// Default wait budget when --timeout is not given, mirroring the daemon.
const WAIT_TIMEOUT_DEFAULT: &str = "60s";

/// Slack added to the socket read deadline over the daemon-side wait
/// budget, so the transport never times out before the daemon answers.
const WAIT_READ_SLACK: Duration = Duration::from_secs(10);

/// Version plus the commit that built it (build.rs), so "which build am
/// I on" is one command instead of an afternoon.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CYCLOPS_BUILD_REF"),
    ")"
);

#[derive(Parser)]
#[command(
    name = "cyclops",
    version = VERSION,
    about = "One eye on every agent",
    after_help = "With no command, opens the full-screen workspace (and starts the daemon if needed)."
)]
struct Cli {
    /// Print raw results as JSON. Anything the UI shows, scripts can read.
    #[arg(long, global = true)]
    json: bool,

    /// No color, no glyph animation. Screen-reader friendly.
    #[arg(long, global = true)]
    plain: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the default workspace: restore it, or build it from a preset.
    /// Safe to run twice; a session that is already there is left alone.
    Start(StartArgs),
    /// Save and restore the shape of a session: panes, sizes, names.
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// What cyclops is watching and the state of every agent.
    Status,
    /// Name a pane so cyclops can address it. `--clear` gives it back.
    Name(NameArgs),
    /// Every named agent: what it is called, how it is doing, what it is on.
    /// Inside tmux it scopes to your session; `--all` is every session.
    List(ListArgs),
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
        /// With --source detection: also print the raw pane capture the
        /// sensors read, under the readings. Debugging a manifest needs
        /// both halves of that moment in one look.
        #[arg(long)]
        raw: bool,
    },
    /// Live stream of daemon events and the admin TUI. `--json` prints one
    /// event per line; without it, opens the stream TUI (formerly `ui`).
    Watch {
        /// Only these event kinds (prefix match), comma separated. JSON mode
        /// only; ignored when opening the TUI.
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        #[command(flatten)]
        ui: UiArgs,
    },
    /// Send a message. The receipt names each delivery's state; exit 0 on
    /// delivered/queued, 1 on parked or needs attention.
    Send(SendArgs),
    /// Messages from the record, newest last. Filter by agent or direction.
    History(HistoryArgs),
    /// One message with its replies and delivery record, oldest first.
    Thread {
        /// Message id, e.g. m-3f9c2a.
        id: String,
    },
    /// Wait for an agent to reach a state. Exit 0 when reached, 2 on
    /// timeout, 3 when the pane died or changed occupant mid-wait.
    Wait {
        /// Agent label or pane id, e.g. reviewer or %4.
        target: String,
        /// idle: composer ready. done: the current or next turn ends.
        /// blocked: any blocked state (modal, permission, quota).
        #[arg(long, value_enum)]
        until: UntilArg,
        /// Give up after this long, e.g. 90s, 2m, 1m30s. Max 10m.
        #[arg(long, default_value = WAIT_TIMEOUT_DEFAULT)]
        timeout: String,
    },
    /// Deprecated alias for `cyclops watch`. Use `cyclops watch` instead.
    Ui(UiArgs),
    /// Relay a vendor hook event to cyclops. Silent, always exits 0.
    Hook {
        /// Event name, e.g. Stop. An argument because agy payloads carry
        /// no event-name field (F7); the payload arrives on stdin.
        event: String,
        /// Reporting agent label; defaults to $CYCLOPS_AGENT.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Prepare vendor hook configs and prove they fire.
    Hooks {
        #[command(subcommand)]
        cmd: HooksCmd,
    },
    /// Switch themes, or list them with a preview of each.
    Theme {
        /// Theme to switch to, e.g. light. Omit to list what is there.
        name: Option<String>,
    },
    /// Update cyclops itself: fetch the source, rebuild, and replace the
    /// installed binaries. Config, themes, manifests and the record are
    /// untouched. Prints old and new build, then restarts the daemon when
    /// nothing is mid-flight; an open workspace is never touched. (Wiring
    /// agent hooks is `cyclops hooks install`, a different job.)
    Update,
    /// The daemon: stop it, ask after it, read its log. `cyclops start`
    /// starts one for you, so there is no `daemon start`.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Whether one is running, since when, and where its log is.
    Status,
    /// Stop it. Your tmux sessions and the record are untouched.
    Stop,
    /// Stop it and start it again on the binaries installed now. Refuses
    /// while a delivery is mid-flight; messages that have not reached a
    /// pane ride through.
    Restart,
    /// Print the daemon's log, which is where a detached one writes.
    Log {
        /// How many lines from the end.
        #[arg(long, default_value = "40")]
        lines: usize,
    },
}

#[derive(clap::Args)]
struct StartArgs {
    /// Workspace to open. Defaults to the config's default_workspace.
    #[arg(long)]
    workspace: Option<String>,
    /// Session to open it in. Defaults to the workspace name.
    #[arg(long)]
    session: Option<String>,
    /// Which shipped arrangement to build when nothing is saved:
    /// solo, duo, quad, or ops. Ignored once the session exists.
    #[arg(long)]
    preset: Option<String>,
    /// Run each pane's recorded command instead of leaving a shell.
    #[arg(long)]
    launch: bool,
    /// Agent CLIs to start, by manifest id, one per named pane in layout
    /// order: --agents claude,codex. They run as cyclops builds the panes;
    /// a later start or restore still needs --launch to run them again.
    #[arg(long, value_delimiter = ',')]
    agents: Vec<String>,
    /// Write the config and the detection manifests, and stop before
    /// opening anything. What `scripts/install.sh` runs last.
    #[arg(long)]
    setup_only: bool,
    /// Also put cyclops' hook entries in the config each installed agent
    /// CLI reads on its own, so a fresh install reports turn edges and
    /// earns verified receipts instead of detecting agents it never hears
    /// from. Merges around whatever is already there and copies the
    /// original aside first.
    ///
    /// Opt-in, and only the installer opts in. Writing into another tool's
    /// configuration is not something a bare `--setup-only` should do to
    /// someone who only asked for a usable home, and a default-on version
    /// of this edits the real ~/.codex from inside a test run.
    /// CYCLOPS_NO_VENDOR_HOOKS=1 declines it for an install that wants
    /// nothing of the sort.
    ///
    /// The consent is recorded ($CYCLOPS_HOME/vendor-wiring-consented), so
    /// an agent CLI installed after cyclops still gets wired: the next
    /// ordinary `cyclops` or `cyclops start` finishes the job. Delete the
    /// marker to withdraw that.
    #[arg(long, requires = "setup_only")]
    wire_hooks: bool,
    /// Do not start cyclopsd. For running the daemon under your own
    /// supervisor, and for a workspace you want open with nothing
    /// watching it. Without this, `start` starts one when none answers.
    #[arg(long)]
    no_daemon: bool,
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    /// Write the session's shape, names and directories to a file.
    Save {
        /// Workspace name. Defaults to the session's.
        name: Option<String>,
        /// Session to read. Defaults to the default workspace's.
        #[arg(long)]
        session: Option<String>,
    },
    /// Build a saved workspace again, in a new session.
    Restore {
        /// Workspace to restore. Defaults to the config's default_workspace.
        name: Option<String>,
        /// Session to build. Defaults to the workspace name.
        #[arg(long)]
        session: Option<String>,
        /// Run each pane's recorded command instead of leaving a shell.
        #[arg(long)]
        launch: bool,
    },
}

#[derive(Subcommand)]
enum HooksCmd {
    /// Render a hook config for one CLI and print the wiring instructions.
    /// Never writes into vendor config directories.
    Install {
        /// Which agent CLI to render for.
        cli: hookset::CliKind,
        /// Cyclops label the hooks report as.
        #[arg(long)]
        agent: String,
        /// Print what would be written without writing it.
        #[arg(long)]
        dry_run: bool,
        /// Output directory; defaults to $CYCLOPS_HOME/hooks/<label>/.
        #[arg(long)]
        dest: Option<std::path::PathBuf>,
    },
    /// Hook liveness for a pane: tier and last-seen edge ages.
    Verify {
        /// Agent label or pane id.
        target: String,
    },
    /// One no-op round trip through the delivery pipeline, reporting
    /// whether the ack hook fired with the marker. Costs one trivial turn.
    Selftest {
        /// Agent label or pane id.
        target: String,
    },
}

#[derive(clap::Args)]
struct UiArgs {
    /// Start in the firehose: every message and state event live.
    #[arg(long)]
    firehose: bool,
    /// Only entries involving this agent (either direction).
    #[arg(long, conflicts_with_all = ["from", "to"])]
    with: Option<String>,
    /// Only messages from this sender.
    #[arg(long)]
    from: Option<String>,
    /// Only messages to this recipient.
    #[arg(long)]
    to: Option<String>,
    /// Ledger lines replayed for backfill before going live.
    #[arg(long, default_value_t = 200)]
    backfill: usize,
}

#[derive(clap::Args)]
struct ListArgs {
    /// Every watched session's agents. Without it, a caller inside tmux
    /// sees only the session it is sitting in when several are watched.
    #[arg(long)]
    all: bool,
}

#[derive(clap::Args)]
struct NameArgs {
    /// The pane to name: a tmux pane id like %4, or the name it has now.
    /// With --self this is the name instead, because the pane is this one.
    #[arg(required_unless_present = "self_")]
    target: Option<String>,
    /// What to call it, e.g. reviewer. Omit with --clear.
    #[arg(
        required_unless_present_any = ["clear", "self_"],
        conflicts_with_all = ["clear", "self_"],
    )]
    label: Option<String>,
    /// Name the pane this command is running in: `cyclops name reviewer
    /// --self`. For an agent that registers itself on startup, and for
    /// naming the pane you are sitting in without looking up its id.
    #[arg(long = "self", id = "self_", conflicts_with = "clear")]
    self_: bool,
    /// Which agent CLI is in this pane (claude, codex, agy, cursor). Skip it and
    /// cyclops works it out from the running process.
    #[arg(long, conflicts_with = "clear")]
    manifest: Option<String>,
    /// Take the name back. The pane's tmux border goes back to yours when
    /// cyclopsd can still reach the pane. When it cannot, the clear fails
    /// and the name is kept: that record holds the only copy of your own
    /// border settings. Run it again once tmux is answering.
    #[arg(long)]
    clear: bool,
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
    /// After delivery, also wait for the recipient: idle, done, or blocked.
    /// The receipt gains a wait outcome per recipient.
    #[arg(long, value_enum)]
    wait: Option<UntilArg>,
    /// Wait budget for --wait, e.g. 90s, 2m. Default 60s, max 10m.
    #[arg(long, requires = "wait", default_value = WAIT_TIMEOUT_DEFAULT)]
    timeout: String,
}

#[derive(clap::Args)]
struct HistoryArgs {
    /// Messages from or to this agent. "me" is you.
    #[arg(long, conflicts_with_all = ["from", "to"])]
    with: Option<String>,
    /// Only messages from this sender. "me" is you.
    #[arg(long)]
    from: Option<String>,
    /// Only messages to this recipient. "me" is you.
    #[arg(long)]
    to: Option<String>,
    /// Most recent N messages (default 50).
    #[arg(long)]
    limit: Option<u32>,
    /// Resume after this record seq (next_cursor from a --json call).
    #[arg(long)]
    cursor: Option<u64>,
}

#[derive(Clone, Copy, ValueEnum)]
enum UntilArg {
    Idle,
    Done,
    Blocked,
}

impl UntilArg {
    fn word(self) -> &'static str {
        match self {
            UntilArg::Idle => "idle",
            UntilArg::Done => "done",
            UntilArg::Blocked => "blocked",
        }
    }
}

impl From<UntilArg> for WaitUntil {
    fn from(u: UntilArg) -> Self {
        match u {
            UntilArg::Idle => WaitUntil::Idle,
            UntilArg::Done => WaitUntil::Done,
            UntilArg::Blocked => WaitUntil::Blocked,
        }
    }
}

/// Human duration: segments of <number><unit> with units ms, s, m, h
/// ("90s", "2m", "1m30s", "500ms"); a bare number means seconds. Zero and
/// anything unparseable are rejected.
fn parse_duration(input: &str) -> Result<Duration, ()> {
    let s = input.trim();
    if s.is_empty() {
        return Err(());
    }
    if let Ok(secs) = s.parse::<u64>() {
        return if secs == 0 {
            Err(())
        } else {
            Ok(Duration::from_secs(secs))
        };
    }
    let mut total = Duration::ZERO;
    let mut rest = s;
    while !rest.is_empty() {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return Err(());
        }
        let (num, tail) = rest.split_at(digits);
        let n: u64 = num.parse().map_err(|_| ())?;
        let unit = tail.len()
            - tail
                .trim_start_matches(|c: char| c.is_ascii_alphabetic())
                .len();
        let (unit, tail) = tail.split_at(unit);
        total += match unit {
            "ms" => Duration::from_millis(n),
            "s" => Duration::from_secs(n),
            "m" => Duration::from_secs(n * 60),
            "h" => Duration::from_secs(n * 3600),
            _ => return Err(()),
        };
        rest = tail;
    }
    if total.is_zero() {
        Err(())
    } else {
        Ok(total)
    }
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

/// Human output style, built only where something is about to be
/// rendered. [`Style::detect`] reads the theme file and prints its
/// warnings, so a command that renders nothing must never call it: see the
/// hook and ui arms in [`run`].
fn style_for(cli: &Cli) -> Style {
    // Machine output never decorates, whatever the terminal supports.
    if cli.json {
        Style::none()
    } else {
        Style::detect(cli.plain)
    }
}

fn run(cli: &Cli) -> i32 {
    match &cli.cmd {
        None => {
            if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
                seed_home_for_workspace();
                ensure_daemon_for_workspace();
                cyclops_workspace::run()
            } else {
                cyclops_workspace::print_help_and_exit()
            }
        }
        Some(cmd) => run_cmd(cli, cmd),
    }
}

/// Bare `cyclops` is a front door the same as `cyclops start`, so it seeds
/// the shipped themes and detection manifests the same way before the
/// workspace opens. Without themes, a config or `$CYCLOPS_THEME` naming a
/// shipped theme was a missing file and a warning about fixing one that was
/// never there. Without manifests, every pane reads unknown and nothing can
/// be delivered — indistinguishable from a broken install to a first-time
/// visitor who ran bare `cyclops` after a binary-only copy. Existing files
/// are never overwritten, so running on every open costs nothing.
///
/// A problem is a note, not an exit: a home without themes still renders in
/// built-in colors, and a home without manifests still opens (the sidebar
/// shows unknown) rather than refusing the front door.
fn seed_home_for_workspace() {
    let home = cyclops_proto::cyclops_home();
    for why in themeseed::seed(&home).problems {
        eprintln!("{why}");
    }
    let seeded = manifests::seed(&home);
    if seeded.none_installed() {
        eprintln!("{}", manifests::nothing_installed(&seeded));
    } else if !seeded.problems.is_empty() {
        eprintln!("{}", manifests::partly_installed(&seeded));
    }
    // The vendor homes too, when the installer's consent is on file: an
    // agent CLI installed after cyclops gets its skill and hook config on
    // this boot instead of never. Quiet unless something was written, so
    // the front door stays silent on every ordinary open.
    for note in workspace::finish_deferred_wiring(&home) {
        eprintln!("{note}");
    }
}

/// Bare `cyclops` opens the workspace, and the workspace is only decorated
/// by what the daemon reports: without one, every agent reads unknown, no
/// pane is detected, and no state ever changes. That is indistinguishable
/// from a broken build, so the front door starts a daemon the same way
/// `cyclops start` does rather than leaving it to a second command.
///
/// Unlike `start`, this runs before the session exists, because the
/// workspace creates or attaches its own session after this returns. The
/// cost is the daemon's attach retry rather than an immediate attach; it
/// converges within seconds, and the workspace asks it to watch whatever
/// session it lands on (`session.watch`) regardless of what was configured.
///
/// A failure is a note, not an exit. The workspace is still usable without
/// a daemon, and its sidebar says `cyclopsd offline` for as long as none
/// answers, so the state is never silently wrong. Boot failures write their
/// own reason to the daemon log; this only has to carry the ones that never
/// got as far as a running daemon.
fn ensure_daemon_for_workspace() {
    let home = cyclops_proto::cyclops_home();
    if let Err(why) = daemon::ensure_running(&home) {
        eprintln!("{why}");
    }
}

fn run_cmd(cli: &Cli, cmd: &Cmd) -> i32 {
    match cmd {
        // Hook never prints and owns its transport handling: a hook that
        // fails loudly breaks the vendor CLI that invoked it. No Style is
        // built on this path, so a broken theme file cannot put a warning
        // on the vendor CLI's stderr.
        Cmd::Hook { event, agent } => hook::run(event, agent.as_deref()),
        // The stream UI owns its own daemon connections, terminal, and
        // theme (cyclops-ui's Theme::detect); dispatch only hands the
        // flags over. Building a Style here warned about the same file
        // twice.
        Cmd::Ui(args) => cmd_ui(cli, args),
        Cmd::Watch { kinds, ui } => cmd_watch(cli, &style_for(cli), kinds, ui),
        // The workspace verbs talk to tmux, and reach the daemon only for
        // the labels. A down daemon costs them the names, not the verb, so
        // they must not go through connect().
        Cmd::Start(args) if args.setup_only => {
            workspace::run_setup(cli.json, &style_for(cli), args.wire_hooks)
        }
        Cmd::Start(args) => workspace::run_start(
            cli.json,
            &style_for(cli),
            args.workspace.as_deref(),
            args.session.as_deref(),
            args.preset.as_deref(),
            &workspace::Launch {
                stored: args.launch,
                agents: &args.agents,
            },
            !args.no_daemon,
        ),
        Cmd::Workspace {
            cmd: WorkspaceCmd::Save { name, session },
        } => workspace::run_save(
            cli.json,
            &style_for(cli),
            name.as_deref(),
            session.as_deref(),
        ),
        Cmd::Workspace {
            cmd:
                WorkspaceCmd::Restore {
                    name,
                    session,
                    launch,
                },
        } => workspace::run_restore(
            cli.json,
            &style_for(cli),
            name.as_deref(),
            session.as_deref(),
            *launch,
        ),
        // Theme reads and writes files. It nudges a running daemon so a
        // switch is live at once, but a down daemon costs it only that,
        // so it must not go through connect() either.
        Cmd::Theme { name } => theme::run(cli.json, &style_for(cli), name.as_deref()),
        // Update replaces the binaries on disk and never talks to the
        // daemon: the daemon keeps running the old build until the
        // restart steps it prints are followed, and saying so is part of
        // its output.
        Cmd::Update => update::run(cli.json, &style_for(cli)),
        // All three answer about a daemon rather than through one, so a
        // daemon that is down is an answer here, not a failure.
        Cmd::Daemon { cmd } => cmd_daemon(cli, &style_for(cli), cmd),
        // Send and wait validate usage before touching the daemon, so
        // usage errors don't hide behind a down daemon.
        Cmd::Send(args) => cmd_send(cli, &style_for(cli), args),
        Cmd::Wait {
            target,
            until,
            timeout,
        } => cmd_wait(cli, &style_for(cli), target, *until, timeout),
        // Install renders and instructs without a daemon; verify and
        // selftest ask the daemon for hook liveness.
        Cmd::Hooks {
            cmd:
                HooksCmd::Install {
                    cli: kind,
                    agent,
                    dry_run,
                    dest,
                },
        } => hookset::run_install(*kind, agent, *dry_run, dest.as_deref(), cli.json),
        // Which pane, and what to call it, are settled before anything
        // connects: a name nobody can use is a usage error, and the rule
        // above says those must not hide behind a down daemon.
        Cmd::Name(args) => {
            let (target, label) = match resolve_name(args) {
                Ok(v) => v,
                Err(code) => return code,
            };
            let mut c = match connect() {
                Ok(c) => c,
                Err(code) => return code,
            };
            cmd_name(
                &mut c,
                cli,
                &style_for(cli),
                args,
                &target,
                label.as_deref(),
            )
        }
        Cmd::Status
        | Cmd::List(_)
        | Cmd::Ping
        | Cmd::Read { .. }
        | Cmd::History(_)
        | Cmd::Thread { .. }
        | Cmd::Hooks { .. } => {
            let mut c = match connect() {
                Ok(c) => c,
                Err(code) => return code,
            };
            let style = style_for(cli);
            match cmd {
                Cmd::Status => cmd_status(&mut c, cli, &style),
                Cmd::List(args) => cmd_list(&mut c, cli, &style, args),
                Cmd::Ping => cmd_ping(&mut c, cli, &style),
                Cmd::Read {
                    target,
                    lines,
                    source,
                    raw,
                } => cmd_read(&mut c, cli, &style, target, *lines, (*source).into(), *raw),
                Cmd::History(args) => cmd_history(&mut c, cli, &style, args),
                Cmd::Thread { id } => cmd_thread(&mut c, cli, &style, id),
                Cmd::Hooks {
                    cmd: HooksCmd::Verify { target },
                } => hookset::run_verify(&mut c, cli.json, &style, target),
                Cmd::Hooks {
                    cmd: HooksCmd::Selftest { target },
                } => hookset::run_selftest(&mut c, cli.json, &style, target),
                Cmd::Send(_)
                | Cmd::Hook { .. }
                | Cmd::Name(_)
                | Cmd::Daemon { .. }
                | Cmd::Wait { .. }
                | Cmd::Hooks { .. }
                | Cmd::Ui(_)
                | Cmd::Watch { .. }
                | Cmd::Start(_)
                | Cmd::Theme { .. }
                | Cmd::Update
                | Cmd::Workspace { .. } => {
                    unreachable!("handled above")
                }
            }
        }
    }
}

/// cyclops watch: stream TUI by default; `--json` is the machine stream.
fn cmd_watch(cli: &Cli, style: &Style, kinds: &[String], ui: &UiArgs) -> i32 {
    if cli.json {
        let mut c = match connect() {
            Ok(c) => c,
            Err(code) => return code,
        };
        return cmd_watch_json(&mut c, cli, style, kinds);
    }
    run_stream_ui(cli, ui)
}

/// cyclops ui: deprecated alias for `cyclops watch`.
fn cmd_ui(cli: &Cli, args: &UiArgs) -> i32 {
    if cli.json {
        eprintln!("{}", copy::UI_NO_JSON);
        return EXIT_USAGE;
    }
    eprintln!("{}", copy::UI_DEPRECATED);
    run_stream_ui(cli, args)
}

fn run_stream_ui(cli: &Cli, args: &UiArgs) -> i32 {
    cyclops_ui::run(cyclops_ui::UiOptions {
        plain: cli.plain,
        firehose: args.firehose,
        with: args.with.clone(),
        from: args.from.clone(),
        to: args.to.clone(),
        backfill: args.backfill,
    })
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

/// Ask the daemon `method` and decode the reply with `decode`. `Ok(None)`
/// means `--json` already printed the raw answer and the caller returns
/// 0; `Err(code)` means the failure was already printed and the caller
/// returns `code`.
///
/// `decode` is always `serde_json::from_value`, passed in rather than
/// called here because naming the `DeserializeOwned` bound would need a
/// direct serde dependency this crate does not otherwise have; the call
/// site's `let x: T` supplies the concrete type instead.
fn ask<T>(
    c: &mut Client,
    method: &str,
    params: Value,
    json: bool,
    asked: Option<&str>,
    decode: fn(Value) -> serde_json::Result<T>,
) -> Result<Option<T>, i32> {
    let result = match c.request(method, params) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, asked));
            return Err(1);
        }
    };
    if json {
        println!("{result}");
        return Ok(None);
    }
    match decode(result) {
        Ok(v) => Ok(Some(v)),
        Err(_) => {
            eprintln!("{}", copy::UNREADABLE_ANSWER);
            Err(1)
        }
    }
}

/// `status` params for a surface that SHOWS the eye.
///
/// Half the rule is the open-delivery backlog, and the daemon folds it
/// only for a caller that asks (`cyclops_proto::StatusParams`). Asking
/// with an empty object served the pane half alone, so this grid counted
/// blocked panes while `cyclops ui` counted both against the same daemon
/// at the same instant, and the two eyes contradicted each other.
fn eye_status_params() -> Value {
    serde_json::to_value(cyclops_proto::StatusParams {
        open_deliveries: true,
    })
    .expect("status params serialize")
}

fn cmd_status(c: &mut Client, cli: &Cli, style: &Style) -> i32 {
    let status: StatusResult = match ask(
        c,
        "status",
        eye_status_params(),
        cli.json,
        None,
        serde_json::from_value,
    ) {
        Ok(Some(s)) => s,
        Ok(None) => return 0,
        Err(code) => return code,
    };
    let config = cyclops_proto::cyclops_home().join("config.toml");
    println!("{}", render::render_status(&status, style, &config));
    0
}

/// cyclops name <target> <label> [--manifest <id>] [--clear].
///
/// Adoption is explicit and this is the verb that does it: the daemon
/// writes the pane into its registry, records a line on the ledger, and
/// paints the pane's tmux border. `--clear` asks for all three back.
///
/// The badge answers for the name only. Putting a pane's own border
/// format back is the daemon's half, it happens only while the daemon can
/// still reach the pane, and this line must never be read as having
/// confirmed it: see `--clear`'s help.
/// `cyclops daemon`: the three questions about the process itself.
///
/// There is no `daemon start` on purpose. `cyclops start` starts one when
/// none is running, so a verb for it would be a second way to do the same
/// thing and a second thing to know about.
fn cmd_daemon(cli: &Cli, style: &Style, cmd: &DaemonCmd) -> i32 {
    let home = cyclops_proto::cyclops_home();
    let log = daemon::log_path(&home);
    match cmd {
        DaemonCmd::Status => {
            let mut client = match client::Client::connect() {
                Ok(c) => c,
                Err(_) => {
                    if cli.json {
                        println!(
                            "{}",
                            json!({"running": false, "log": log.display().to_string()})
                        );
                    } else {
                        println!("{}", copy::DAEMON_DOWN);
                        println!("  {}", style.dim(&format!("log: {}", log.display())));
                    }
                    // Down is the answer to the question, not a failure to
                    // answer it.
                    return 0;
                }
            };
            let status: StatusResult = match ask(
                &mut client,
                "status",
                json!({}),
                cli.json,
                None,
                serde_json::from_value,
            ) {
                Ok(Some(s)) => s,
                Ok(None) => return 0,
                Err(code) => return code,
            };
            println!("{}", render::daemon_running(&status, style));
            println!("  {}", style.dim(&format!("log: {}", log.display())));
            0
        }
        DaemonCmd::Stop => match daemon::stop() {
            Ok(pid) => {
                if cli.json {
                    println!("{}", json!({"stopped": true, "pid": pid}));
                } else {
                    println!("{}", render::daemon_stopped(pid, style));
                }
                0
            }
            Err(why) => {
                eprintln!("{why}");
                1
            }
        },
        DaemonCmd::Restart => match daemon::restart(&home) {
            Ok(old_pid) => {
                if cli.json {
                    println!("{}", json!({"restarted": true, "stopped_pid": old_pid}));
                } else {
                    println!("{}", render::daemon_restarted(old_pid, style));
                }
                0
            }
            Err(refusal) => {
                eprintln!("{}", refusal.why());
                // A daemon too old for the quiesce handshake cannot be
                // restarted by this verb at all, so the fix is named here
                // rather than left to a retry that can only fail again.
                if matches!(refusal, daemon::RestartRefusal::Predates) {
                    eprintln!("{}", copy::RESTART_PREDATES_FIX);
                }
                1
            }
        },
        DaemonCmd::Log { lines } => match std::fs::read_to_string(&log) {
            Ok(text) => {
                let all: Vec<&str> = text.lines().collect();
                for l in all.iter().skip(all.len().saturating_sub(*lines)) {
                    println!("{l}");
                }
                0
            }
            Err(_) => {
                eprintln!("{}", copy::no_daemon_log(&log));
                1
            }
        },
    }
}

/// Which pane `cyclops name` is about, and what it will be called.
///
/// Runs before the daemon is asked anything, because both answers are
/// knowable here and a bad one is a usage error.
///
/// `--self` moves the positional along: the pane is this one, so the
/// single argument is the name. tmux puts the pane id in the environment
/// of every process it starts, which is how a pane knows which one it is
/// without asking anybody.
fn resolve_name(args: &NameArgs) -> Result<(String, Option<String>), i32> {
    let (target, label) = if args.self_ {
        match std::env::var("TMUX_PANE") {
            Ok(p) if !p.is_empty() => (p, args.target.clone()),
            _ => {
                eprintln!(
                    "{}",
                    copy::self_outside_tmux(args.target.as_deref().unwrap_or("<name>"))
                );
                return Err(EXIT_USAGE);
            }
        }
    } else {
        (args.target.clone().unwrap_or_default(), args.label.clone())
    };

    // Same rule and same sentence as the daemon: cyclops_proto::label.
    if !args.clear {
        if let Some(why) = label.as_deref().and_then(cyclops_proto::label::refusal) {
            eprintln!("{why}");
            return Err(EXIT_USAGE);
        }
    }
    Ok((target, label))
}

fn cmd_name(
    c: &mut Client,
    cli: &Cli,
    style: &Style,
    args: &NameArgs,
    target: &str,
    label: Option<&str>,
) -> i32 {
    let params = json!({
        "target": target,
        "label": if args.clear { Value::Null } else { json!(label) },
        "manifest": args.manifest,
    });
    let result = match c.request("pane.label", params) {
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
    println!("{}", render::render_named(&result, style));
    0
}

/// cyclops list: the roster, one named agent per row, under a header
/// naming the watched session(s) and the home whose socket answered.
///
/// Everything it shows is already in one `status` answer, so there is no
/// second question to ask the daemon and no second place the roster can
/// come from. `status` shows every watched pane; this shows the ones with
/// names.
///
/// The home is this client's own resolution, not a daemon field: it is the
/// directory the socket lives under, so it names the daemon that answered
/// by construction. Two daemons on two homes give two different rosters,
/// and the header is how a reader in the wrong terminal tab finds out.
///
/// Inside tmux the roster scopes to the caller's own session: a fresh tab
/// in session A means "my team here", not everything the daemon watches,
/// and the header plus a note keep the elision honest. `--all`, a caller
/// outside tmux, and a pane the daemon does not watch all get the full
/// roster, byte for byte what it always was. `--json` scopes identically
/// (parity, not a second shape).
fn cmd_list(c: &mut Client, cli: &Cli, style: &Style, args: &ListArgs) -> i32 {
    let result = match c.request("status", json!({})) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, None));
            return 1;
        }
    };
    let mut status: StatusResult = match serde_json::from_value(result) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("{}", copy::UNREADABLE_ANSWER);
            return 1;
        }
    };
    let mut also_watching: Vec<String> = Vec::new();
    if !args.all {
        if let Some(keep) = caller_session(&status, std::env::var("TMUX_PANE").ok().as_deref()) {
            let kept = status.sessions.remove(keep);
            also_watching = std::mem::take(&mut status.sessions)
                .into_iter()
                .map(|s| s.name)
                .collect();
            status.sessions = vec![kept];
        }
    }
    let home = cyclops_proto::cyclops_home();
    if cli.json {
        // Parity, not a second shape: the same rows the grid prints, as
        // the pane records they came from. The header's facts ride along
        // as additive fields, so a script can also tell which rig
        // answered.
        let named: Vec<&PaneStatus> = status
            .sessions
            .iter()
            .flat_map(|s| s.panes.iter())
            .filter(|p| p.agent.is_some())
            .collect();
        let sessions: Vec<&str> = status.sessions.iter().map(|s| s.name.as_str()).collect();
        let mut answer = json!({
            "agents": serde_json::to_value(&named).expect("panes serialize"),
            "home": home.display().to_string(),
            "sessions": sessions,
        });
        // The note's fact, additive and only when something was elided,
        // so an unscoped answer stays byte-identical to what it was.
        if !also_watching.is_empty() {
            answer["also_watching"] = json!(also_watching);
        }
        println!("{answer}");
        return 0;
    }
    println!(
        "{}",
        render::render_list(&status, style, &home, &also_watching)
    );
    0
}

/// The session the caller is sitting in, when that is knowable and
/// unambiguous: tmux puts the pane id in `TMUX_PANE` for every process it
/// starts (the same variable `cyclops name --self` reads), and exactly
/// one watched session holds that pane id.
///
/// Pane ids are unique per tmux SERVER, not per machine. A caller inside
/// a second server (`tmux -L other`) can carry a TMUX_PANE that collides
/// with a watched pane id on the daemon's server; the roster then scopes
/// to a session the caller is not in, which is the pre-scoping failure
/// mode with an honest header. Requiring exactly one match keeps the
/// same-server case right and refuses the one ambiguity that is
/// detectable from here.
fn caller_session(status: &StatusResult, pane: Option<&str>) -> Option<usize> {
    let pane = pane.filter(|p| !p.is_empty())?;
    let mut hits = status
        .sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| s.panes.iter().any(|p| p.pane_id == pane))
        .map(|(i, _)| i);
    match (hits.next(), hits.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
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
    raw: bool,
) -> i32 {
    if raw && source != PaneReadSource::Detection {
        eprintln!("{}", copy::RAW_NEEDS_DETECTION);
        return EXIT_USAGE;
    }
    let params = serde_json::to_value(PaneReadParams {
        target: target.to_string(),
        source,
        lines,
        include_raw: raw,
    })
    .expect("pane.read params serialize");
    let read: PaneReadResult = match ask(
        c,
        "pane.read",
        params,
        cli.json,
        Some(target),
        serde_json::from_value,
    ) {
        Ok(Some(r)) => r,
        Ok(None) => return 0,
        Err(code) => return code,
    };
    if let Some(det) = &read.detection {
        println!(
            "{}",
            render::render_detection(&read.target, det, style, render::now_ms())
        );
        // --raw: the capture the sensors read, under the readings. Same
        // answer, same moment; a second read could straddle a change.
        if let Some(text) = &read.text {
            println!();
            println!(
                "{}",
                style.dim(&format!("what the sensors read ({}):", read.pane_id))
            );
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
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
    // --wait composes send-and-wait; the timeout parses before connecting.
    let wait = match &args.wait {
        None => None,
        Some(until) => {
            let Ok(budget) = parse_duration(&args.timeout) else {
                eprintln!("{}", copy::bad_duration(&args.timeout));
                return EXIT_USAGE;
            };
            Some(WaitSpec {
                until: (*until).into(),
                timeout_ms: Some(budget.as_millis() as u64),
            })
        }
    };
    let mut c = match connect() {
        Ok(c) => c,
        Err(code) => return code,
    };
    if let Some(spec) = &wait {
        // The daemon holds the response until delivery plus wait resolve.
        c.set_read_timeout(
            Duration::from_millis(spec.timeout_ms.unwrap_or_default()) + WAIT_READ_SLACK,
        );
    }
    let params = serde_json::to_value(MsgSendParams {
        to: to.clone(),
        subject: args.subject.clone(),
        body,
        fyi: args.fyi,
        reply_to: args.reply_to.clone(),
        wait,
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
    let waits: Vec<Value> = result
        .get("wait")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let receipt: MsgSendResult = match serde_json::from_value(result) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("{}", copy::UNREADABLE_ANSWER);
            return 1;
        }
    };
    println!("{}", render::render_receipts(&receipt.deliveries, style));
    if !waits.is_empty() {
        println!("{}", render::render_wait_entries(&waits, style));
    }
    // A badge is a state, not an outcome. Every receipt that is not a
    // delivery gets one line saying what became of the message, because
    // that is the sentence a sender is actually looking for.
    for d in &receipt.deliveries {
        match d.state {
            DeliveryState::ParkedBlockedQuota => {
                eprintln!("{}", copy::parked(&d.to, d.note.as_deref()));
            }
            DeliveryState::AttentionRequired => {
                // The pin command is offered for the one cause it fixes.
                // A dead pane or a name nobody answers to is not taught
                // away with a manifest, and offering it there would send
                // the reader after the wrong thing.
                let pane = (d.note.as_deref() == Some(copy::CAUSE_NO_MANIFEST))
                    .then_some(d.pane.as_deref())
                    .flatten();
                eprintln!(
                    "{}",
                    copy::needs_attention_for(&d.to, pane, d.note.as_deref())
                );
            }
            // Past the paste and still unresolved: the pane has the
            // payload and the confirmation is outstanding.
            DeliveryState::Pasting | DeliveryState::Staged | DeliveryState::Submitted => {
                eprintln!("{}", copy::in_flight(&d.to));
            }
            _ => {}
        }
    }
    receipts_exit(&receipt.deliveries)
}

/// cyclops wait <target> --until idle|done|blocked [--timeout 60s].
/// Blocks on the daemon's agent.wait: the daemon watches the fused state
/// stream and pins the pane occupant; nothing here polls.
fn cmd_wait(cli: &Cli, style: &Style, target: &str, until: UntilArg, timeout: &str) -> i32 {
    let Ok(budget) = parse_duration(timeout) else {
        eprintln!("{}", copy::bad_duration(timeout));
        return EXIT_USAGE;
    };
    let mut c = match connect() {
        Ok(c) => c,
        Err(code) => return code,
    };
    // The daemon holds the response for the whole wait.
    c.set_read_timeout(budget + WAIT_READ_SLACK);
    let params = json!({
        "target": target,
        "until": until.word(),
        "timeout_ms": budget.as_millis() as u64,
    });
    match c.request("agent.wait", params) {
        Ok(v) => {
            if cli.json {
                println!("{v}");
            } else {
                println!(
                    "{}",
                    render::wait_badge(
                        "reached",
                        serde_json::from_value(v["state"].clone()).ok(),
                        v["waited_ms"].as_u64(),
                        None,
                        style,
                    )
                );
            }
            0
        }
        Err(ClientError::Server {
            code,
            message,
            data,
            ..
        }) if code == "timeout" || code == "occupant_changed" => {
            if cli.json {
                println!(
                    "{}",
                    json!({"code": code, "message": message, "data": data})
                );
            } else if code == "timeout" {
                eprintln!(
                    "{}",
                    copy::wait_timeout(target, until.word(), budget, data["state"].as_str())
                );
            } else {
                eprintln!("{}", copy::wait_occupant_changed(target));
            }
            if code == "timeout" {
                EXIT_WAIT_TIMEOUT
            } else {
                EXIT_OCCUPANT_CHANGED
            }
        }
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, Some(target)));
            1
        }
    }
}

/// The record, newest last: folded msg lines rendered on the history grid.
fn cmd_history(c: &mut Client, cli: &Cli, style: &Style, args: &HistoryArgs) -> i32 {
    let params = serde_json::to_value(HistoryParams {
        with: args.with.clone(),
        from: args.from.clone(),
        to: args.to.clone(),
        limit: args.limit.unwrap_or(50),
        cursor: args.cursor,
    })
    .expect("msg.history params serialize");
    let history: HistoryResult = match ask(
        c,
        "msg.history",
        params,
        cli.json,
        None,
        serde_json::from_value,
    ) {
        Ok(Some(h)) => h,
        Ok(None) => return 0,
        Err(code) => return code,
    };
    if history.lines.is_empty() {
        // Empty states invite the next action. Name the filtered agent when
        // there is one worth addressing.
        let target = args.with.as_deref().or(args.to.as_deref());
        let target = target.filter(|t| *t != "me");
        println!("{}", copy::no_messages(target));
        return 0;
    }
    println!(
        "{}",
        render::render_history(&history.lines, style, render::now_ms())
    );
    0
}

/// One thread: the message, its replies, and each delivery's current badge.
fn cmd_thread(c: &mut Client, cli: &Cli, style: &Style, id: &str) -> i32 {
    let thread: ThreadResult = match ask(
        c,
        "msg.thread",
        json!({"id": id}),
        cli.json,
        None,
        serde_json::from_value,
    ) {
        Ok(Some(t)) => t,
        Ok(None) => return 0,
        Err(code) => return code,
    };
    println!(
        "{}",
        render::render_thread(&thread.lines, style, render::now_ms())
    );
    0
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
/// anything the pipeline cannot leave on its own exits 1.
///
/// Which states those are is the rule's delivery half and is not decided
/// here: `cyclops_proto::delivery_needs_human` is the same predicate the
/// eye counts by and the daemon folds `status` by, so an exit code and an
/// eye can never disagree about one delivery.
fn receipts_exit(ds: &[DeliveryReceipt]) -> i32 {
    i32::from(ds.iter().any(|d| delivery_needs_human(d.state)))
}

/// The same rule read tolerantly off the raw result for --json
/// passthrough: a state name from a newer daemon does not decode, and an
/// exit code is not the place to fail on that.
fn receipts_exit_json(v: &Value) -> i32 {
    let bad = v
        .get("deliveries")
        .and_then(Value::as_array)
        .is_some_and(|a| {
            a.iter().any(|d| {
                serde_json::from_value::<DeliveryState>(d["state"].clone())
                    .is_ok_and(delivery_needs_human)
            })
        });
    i32::from(bad)
}

fn cmd_watch_json(c: &mut Client, cli: &Cli, style: &Style, kinds: &[String]) -> i32 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_human_forms() {
        assert_eq!(parse_duration("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_duration("1m30s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("1h"), Ok(Duration::from_secs(3600)));
        assert_eq!(parse_duration(" 2m "), Ok(Duration::from_secs(120)));
    }

    #[test]
    fn bad_durations_are_rejected() {
        for bad in ["", "0", "0s", "nope", "10x", "s", "-5s", "1.5s", "5s junk"] {
            assert_eq!(parse_duration(bad), Err(()), "{bad:?} should not parse");
        }
    }

    fn receipt(state: DeliveryState) -> DeliveryReceipt {
        DeliveryReceipt {
            to: "reviewer".into(),
            state,
            position: None,
            note: None,
            pane: None,
            held_by: None,
        }
    }

    /// The exit code and the eye answer the same question, so they read the
    /// same predicate. Both paths are checked against every state, and the
    /// --json path against the same states as the daemon spells them, so a
    /// state moving between halves of the rule moves both together.
    #[test]
    fn the_exit_code_branches_exactly_where_the_rule_does() {
        use DeliveryState::*;
        for state in [
            Queued,
            Gating,
            Pasting,
            Staged,
            Submitted,
            DeliveredVerified,
            DeliveredUnverified,
            RetryQueued,
            AttentionRequired,
            ParkedBlockedQuota,
        ] {
            let want = i32::from(delivery_needs_human(state));
            assert_eq!(receipts_exit(&[receipt(state)]), want, "{state:?}");
            let wire = serde_json::to_value(state).expect("a state serializes");
            assert_eq!(
                receipts_exit_json(&json!({"deliveries": [{"to": "reviewer", "state": wire}]})),
                want,
                "{state:?} through --json"
            );
        }
        // One bad recipient in a broadcast is enough.
        assert_eq!(
            receipts_exit(&[receipt(DeliveredVerified), receipt(ParkedBlockedQuota)]),
            1
        );
        // A state this build does not know is not an error to report on:
        // the protocol is tolerant, and an exit code is a bad place to
        // fail. Neither is a missing deliveries array.
        assert_eq!(
            receipts_exit_json(&json!({"deliveries": [{"state": "from_next_year"}]})),
            0
        );
        assert_eq!(receipts_exit_json(&json!({})), 0);
    }

    /// A watched roster for the scoping rule: session names, each with
    /// its pane ids. Built off the wire shape so the fixture cannot
    /// drift from what a daemon answers.
    fn watched(sessions: &[(&str, &[&str])]) -> StatusResult {
        let sessions: Vec<Value> = sessions
            .iter()
            .map(|(name, panes)| {
                json!({
                    "name": name, "attached": true,
                    "panes": panes.iter().map(|id| json!({
                        "pane_id": id, "window_id": "@1", "window_name": "w",
                        "title": "", "current_command": "sh", "dead": false,
                        "in_mode": false, "width": 80, "height": 24,
                        "state": "idle"
                    })).collect::<Vec<Value>>()
                })
            })
            .collect();
        serde_json::from_value(json!({
            "daemon_version": "0.1.0", "proto": 1, "boot_id": "b",
            "uptime_ms": 0, "tmux_version": "3.6a", "sessions": sessions
        }))
        .expect("status fixture")
    }

    /// The scoping rule in one place: scope only when the caller's pane
    /// is knowable AND exactly one watched session holds it. Everything
    /// else, including the detectable ambiguity of two sessions claiming
    /// one pane id, falls through to the full roster.
    #[test]
    fn the_roster_scopes_only_on_an_unambiguous_pane_match() {
        let two = watched(&[("main", &["%1", "%2"]), ("ops", &["%7"])]);
        // Outside tmux there is no context to scope by.
        assert_eq!(caller_session(&two, None), None);
        assert_eq!(caller_session(&two, Some("")), None);
        // Inside, the one session holding the pane wins.
        assert_eq!(caller_session(&two, Some("%2")), Some(0));
        assert_eq!(caller_session(&two, Some("%7")), Some(1));
        // A pane the daemon does not watch scopes nothing.
        assert_eq!(caller_session(&two, Some("%99")), None);
        // Two sessions claiming the pane id is the cross-server collision
        // made visible; refusing to pick is the only honest answer.
        let clash = watched(&[("main", &["%1"]), ("ops", &["%1"])]);
        assert_eq!(caller_session(&clash, Some("%1")), None);
    }
}
