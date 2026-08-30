//! cyclops: the thin CLI client for cyclopsd.
//!
//! ## What it owns
//!
//! Two jobs, and nothing between them. It speaks NDJSON over the daemon's
//! Unix socket (`client.rs`, types from `cyclops-proto`), and it renders
//! what comes back for a human (`render.rs` layout, `style.rs` color,
//! `copy.rs` words). Structured daemon reads and direct mutations take
//! `--json` and print exactly the socket answer, which keeps rendering
//! optional. Guarded age-selected alarm clearance is interactive because it
//! must confirm a frozen preview. `update`, `daemon log`, and `cyclops ui`
//! remain text; the machine stream is `cyclops watch --json`.
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
//! The command surface covers live watching, durable messaging, history,
//! lifecycle waits, hook setup, workspace management, themes, installation,
//! and daemon control.

mod cleanup;
mod client;
mod consumer;
mod copy;
mod daemon;
mod hash;
mod health;
mod hook;
mod hookset;
mod manifests;
mod render;
mod setup;
mod sizing;
mod skillseed;
mod soundseed;
mod style;
mod theme;
mod themeseed;
mod update;
mod workspace;

use std::io::{BufRead, IsTerminal, Read, Write};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{json, Value};

use client::{Certainty, Client, ClientError};
use cyclops_proto::{
    delivery_needs_human, DeliveryReceipt, DeliveryState, HistoryParams, HistoryResult,
    MessageNotificationState, MsgSendParams, MsgSendResult, PaneReadParams, PaneReadResult,
    PaneReadSource, PaneStatus, StatusResult, SubscribeParams, ThreadResult, WaitUntil,
    PROTOCOL_VERSION,
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

/// Exact source build stamped by this crate's existing build script.
const BUILD_REF: &str = env!("CYCLOPS_BUILD_REF");

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
    /// Request structured JSON where the command supports it. `watch` emits
    /// NDJSON. `update`, `daemon log`, and the interactive UI remain text.
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
    /// Inspect manifests, hook wiring, and agent skill installation.
    /// Reads setup state without changing it.
    Setup {
        #[command(subcommand)]
        cmd: SetupCmd,
    },
    /// Save and restore the shape of a session: panes, sizes, names.
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// Hand a session's window sizing back to tmux.
    ///
    /// A workspace sizes the windows it owns and restores them when it
    /// quits. Use this when one was killed hard and no workspace is coming
    /// back to tidy up, or when you are finished with Cyclops on a session.
    Sizing {
        #[command(subcommand)]
        cmd: SizingCmd,
    },
    /// What cyclops is watching and the state of every agent.
    Status,
    /// Inspect the installation, daemon, setup, and state without changing them.
    Health,
    /// Inventory or remove bounded rebuildable assets. Dry-run is the default.
    Cleanup {
        /// List one or both asset classes.
        #[arg(value_enum, required = true)]
        assets: Vec<cleanup::AssetClass>,
        /// Remove exact reported assets after lease and identity revalidation.
        #[arg(long)]
        apply: bool,
    },
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
    /// Store a durable message in one or more recipient inboxes.
    Send(SendArgs),
    /// List or claim messages in the authenticated caller's inbox.
    Inbox(InboxArgs),
    /// Body-free inbox, outbound, and delivery state in one workspace snapshot.
    Messages(MessagesArgs),
    /// Reply to a visible message using its sender and thread.
    Reply(ReplyArgs),
    /// Requeue a message by identifier.
    Requeue(RequeueArgs),
    /// Manage exact notification attempts.
    Notification(NotificationArgs),
    /// Preview or clear delivery alarms.
    Alarm(AlarmArgs),
    /// Inspect or resolve an exact staged notification attempt.
    Attention(AttentionArgs),
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
        /// idle: no turn is running (NOT permission to write). turn-ended:
        /// working was observed, then the same occupant reached idle or
        /// idle_with_input. This has no turn or message identity.
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
        /// no event-name field; the payload arrives on stdin.
        event: String,
        /// Optional reporting label assertion. The daemon derives identity
        /// from the authenticated socket peer when omitted.
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
    /// Update Cyclops itself: fetch the source, rebuild, and replace the
    /// installed binaries. Durable records and operator-edited setup files
    /// are preserved. Untouched shipped themes, manifests, skills, and
    /// Cyclops hook entries may be upgraded. Set CYCLOPS_NO_VENDOR_HOOKS=1
    /// to skip vendor hook and skill wiring. A running daemon is safely
    /// restarted; a stopped daemon stays stopped. An open workspace is untouched.
    Update {
        /// Reactivate a replay-proven retained pair. State is not rolled back.
        #[arg(long)]
        rollback: bool,
        /// Internal installer entry point. The directory must contain both
        /// freshly built binaries.
        #[arg(long, hide = true, value_name = "DIR", conflicts_with = "rollback")]
        install_pair: Option<std::path::PathBuf>,
        /// Remove one fully validated managed pair store during uninstall.
        #[arg(long, hide = true, conflicts_with_all = ["rollback", "install_pair"])]
        remove_pair_store: bool,
        /// Installation prefix used with an internal pair-store operation.
        #[arg(long, hide = true, value_name = "DIR")]
        prefix: Option<std::path::PathBuf>,
    },
    /// The daemon: stop it, ask after it, read its log. `cyclops start`
    /// starts one for you, so there is no `daemon start`.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum SizingCmd {
    /// Put every window of a session back on the sizing policy it had, and
    /// clear the ownership mark. Safe to run twice, and a window cyclops
    /// never sized is left exactly as it is.
    Release {
        /// Session to release. Defaults to the one this shell is in.
        #[arg(long)]
        session: Option<String>,
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

#[derive(Subcommand)]
enum SetupCmd {
    /// Report setup and whether messaging uses a doorbell or direct fallback.
    Check,
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
    /// In the TUI, only entries involving this agent (either direction).
    #[arg(long, conflicts_with_all = ["from", "to"])]
    with: Option<String>,
    /// In the TUI, only messages from this sender.
    #[arg(long)]
    from: Option<String>,
    /// In the TUI, only messages to this recipient.
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
    /// Recipient label, pane id, or admin. Merges with --to.
    target: Option<String>,
    /// One line the recipient sees first.
    #[arg(long)]
    subject: String,
    /// Exactly two sentences shown beside the recipient's inbox claim.
    #[arg(long)]
    summary: String,
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
    /// Announcement expecting no reply.
    #[arg(long)]
    fyi: bool,
    /// Sender-scoped idempotency key for exact retries.
    #[arg(long)]
    client_key: Option<String>,
    /// Exit 0 only when every recipient's bounded receipt proves wake
    /// submitted or notified, or an equivalent legacy direct-delivery
    /// boundary. The daemon waits past writing and staging, but never for
    /// agent work or message completion. Nonzero does not undo acceptance.
    #[arg(long)]
    require_wake: bool,
    /// Message id this replies to. Recipient and subject come from the referenced message.
    #[arg(long, conflicts_with_all = ["target", "to", "all", "supersedes"])]
    reply_to: Option<String>,
    /// Replace one unclaimed message before its notification starts writing.
    #[arg(long, conflicts_with = "reply_to")]
    supersedes: Option<String>,
}

#[derive(clap::Args)]
struct InboxArgs {
    #[command(subcommand)]
    cmd: InboxCmd,
}

#[derive(clap::Args)]
struct MessagesArgs {
    /// Recent settled messages to keep beside every active message.
    #[arg(long, default_value_t = 20)]
    recent_settled: u32,
}

#[derive(Subcommand)]
enum InboxCmd {
    /// List pending messages without their bodies.
    List {
        /// Cap the number of returned entries.
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Claim one message and print its payload.
    Claim {
        /// Message identifier to claim.
        message_id: String,
    },
    /// Wait for and claim the oldest pending message over the daemon socket.
    Next {
        /// Only messages from this canonical sender endpoint.
        #[arg(long, value_name = "RECIPIENT_KEY")]
        from: Option<String>,
        /// Stop waiting when no pending message arrives within this duration.
        #[arg(long, default_value = "30s")]
        timeout: String,
    },
}

#[derive(clap::Args)]
struct ReplyArgs {
    /// Message identifier being answered.
    message_id: String,
    /// Exactly two sentences shown beside the recipient's inbox claim.
    #[arg(long)]
    summary: String,
    /// Reply body text.
    #[arg(long, conflicts_with = "body_file")]
    body: Option<String>,
    /// Read the reply body from a file; - reads stdin.
    #[arg(long)]
    body_file: Option<String>,
    /// Sender-scoped idempotency key.
    #[arg(long)]
    client_key: Option<String>,
}

#[derive(clap::Args)]
struct RequeueArgs {
    /// Message identifier to requeue.
    message_id: String,
}

#[derive(clap::Args)]
struct NotificationArgs {
    #[command(subcommand)]
    cmd: NotificationCmd,
}

#[derive(Subcommand)]
enum NotificationCmd {
    /// Withdraw one exact wake before any terminal write.
    Withdraw {
        /// Exact notification attempt identifier.
        attempt_id: cyclops_proto::NotificationAttemptId,
        /// Canonical durable recipient key for this attempt.
        #[arg(long, value_name = "RECIPIENT_KEY")]
        recipient: cyclops_proto::RecipientKey,
    },
}

#[derive(clap::Args)]
struct AlarmArgs {
    #[command(subcommand)]
    cmd: AlarmCmd,
}

#[derive(Subcommand)]
enum AlarmCmd {
    /// Preview alarms older than a duration without changing them.
    Preview {
        /// Minimum alarm age, e.g. 30m or 2h.
        #[arg(long)]
        older_than: String,
    },
    /// Clear exact alarms, directly by id or through a guarded age preview.
    Clear {
        /// Exact alarm identifiers returned by preview.
        #[arg(
            value_name = "ID",
            required_unless_present = "older_than",
            conflicts_with = "older_than"
        )]
        ids: Vec<String>,
        /// Preview alarms at least this old, then confirm the frozen id set.
        #[arg(
            long,
            value_name = "AGE",
            required_unless_present = "ids",
            conflicts_with = "ids"
        )]
        older_than: Option<String>,
    },
}

#[derive(clap::Args)]
struct AttentionArgs {
    #[command(subcommand)]
    cmd: AttentionCmd,
}

#[derive(Subcommand)]
enum AttentionCmd {
    /// Report all safety checks without changing terminal or journal state.
    Show {
        /// Notification attempt id, or a message id with one unresolved attempt.
        id: String,
        /// Print a local expected-versus-observed line diff.
        #[arg(long)]
        diff: bool,
    },
    /// Submit the exact staged notification.
    Complete {
        /// Notification attempt id, or a message id with one unresolved attempt.
        id: String,
    },
    /// Clear the exact staged notification without submitting it.
    Discard {
        /// Notification attempt id, or a message id with one unresolved attempt.
        id: String,
    },
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
    TurnEnded,
    Blocked,
}

impl UntilArg {
    fn wire_word(self) -> &'static str {
        match self {
            UntilArg::Idle => "idle",
            UntilArg::TurnEnded => "turn_ended",
            UntilArg::Blocked => "blocked",
        }
    }

    fn human_word(self) -> &'static str {
        match self {
            UntilArg::TurnEnded => "turn ended",
            other => other.wire_word(),
        }
    }
}

impl From<UntilArg> for WaitUntil {
    fn from(u: UntilArg) -> Self {
        match u {
            UntilArg::Idle => WaitUntil::Idle,
            UntilArg::TurnEnded => WaitUntil::TurnEnded,
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
        let duration = Duration::from_secs(secs);
        return duration_is_usable(duration).then_some(duration).ok_or(());
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
        let segment = match unit {
            "ms" => Duration::from_millis(n),
            "s" => Duration::from_secs(n),
            "m" => Duration::from_secs(n.checked_mul(60).ok_or(())?),
            "h" => Duration::from_secs(n.checked_mul(3_600).ok_or(())?),
            _ => return Err(()),
        };
        total = total.checked_add(segment).ok_or(())?;
        rest = tail;
    }
    duration_is_usable(total).then_some(total).ok_or(())
}

fn duration_is_usable(duration: Duration) -> bool {
    !duration.is_zero() && Instant::now().checked_add(duration).is_some()
}

/// Protocol durations use unsigned millisecond fields. A duration can fit
/// [`Duration`] while exceeding that wire range, so the conversion is checked.
fn parse_wire_duration_ms(input: &str) -> Result<u64, ()> {
    u64::try_from(parse_duration(input)?.as_millis()).map_err(|_| ())
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
/// be delivered, which is indistinguishable from a broken install to a
/// first-time visitor who ran bare `cyclops` after a binary-only copy.
/// Operator-edited files stay unchanged. Known unedited shipped files may
/// advance, and a current home costs no writes on open.
///
/// A problem is a note, not an exit: a home without themes still renders in
/// built-in colors, and a home without manifests still opens (the sidebar
/// shows unknown) rather than refusing the front door.
fn seed_home_for_workspace() {
    let home = cyclops_proto::cyclops_home();
    for why in themeseed::seed(&home).problems {
        eprintln!("{why}");
    }
    for why in soundseed::seed(&home).problems {
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
        Cmd::Setup {
            cmd: SetupCmd::Check,
        } => setup::run_check(cli.json, &style_for(cli)),
        // Health must not load a theme through an unchecked state path.
        Cmd::Sizing {
            cmd: SizingCmd::Release { session },
        } => match session
            .clone()
            .or_else(|| cyclops_tmux::current_session(None))
        {
            Some(session) => sizing::run_release(
                &cyclops_proto::cyclops_home(),
                &session,
                cli.json,
                &style_for(cli),
            ),
            None => {
                eprintln!(
                    "{}",
                    style_for(cli).bold("not inside tmux: name the session with --session <name>")
                );
                2
            }
        },
        Cmd::Health => health::run(cli.json),
        // Cleanup has no arbitrary path input and does not need the daemon.
        Cmd::Cleanup { assets, apply } => cleanup::run(cli.json, assets, *apply),
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
        // Update validates a matched pair, then asks the exact authenticated
        // daemon generation to stop before one selector changes.
        Cmd::Update {
            rollback,
            install_pair,
            remove_pair_store,
            prefix,
        } => update::run(
            cli.json,
            &style_for(cli),
            *rollback,
            install_pair.as_deref(),
            *remove_pair_store,
            prefix.as_deref(),
        ),
        // All three answer about a daemon rather than through one, so a
        // daemon that is down is an answer here, not a failure.
        Cmd::Daemon { cmd } => cmd_daemon(cli, &style_for(cli), cmd),
        // Message bodies and identifiers are validated before connecting.
        Cmd::Send(args) => cmd_send(cli, &style_for(cli), args),
        Cmd::Reply(args) => cmd_reply(cli, &style_for(cli), args),
        Cmd::Wait {
            target,
            until,
            timeout,
        } => cmd_wait(cli, &style_for(cli), target, *until, timeout),
        Cmd::Inbox(InboxArgs {
            cmd: InboxCmd::Next { timeout, .. },
        }) if parse_duration(timeout).is_err() => inbox_next_failed(
            cli,
            "bad_duration",
            copy::bad_duration(timeout),
            json!({"value": timeout}),
            EXIT_USAGE,
        ),
        Cmd::Inbox(InboxArgs {
            cmd: InboxCmd::Next {
                from: Some(from), ..
            },
        }) if from.parse::<cyclops_proto::RecipientKey>().is_err() => {
            let error = from
                .parse::<cyclops_proto::RecipientKey>()
                .expect_err("guard rejects invalid sender keys");
            inbox_next_failed(
                cli,
                "invalid_recipient_key",
                error.to_string(),
                json!({"value": from}),
                EXIT_USAGE,
            )
        }
        Cmd::Inbox(InboxArgs {
            cmd: InboxCmd::Next { timeout, from },
        }) => {
            let mut client = match Client::connect() {
                Ok(client) => client,
                Err(error) => return inbox_next_client_failed(cli, &error),
            };
            report_hello_mismatch(client.hello());
            cmd_inbox_next(&mut client, cli, timeout, from.as_deref())
        }
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
        | Cmd::Inbox(_)
        | Cmd::Messages(_)
        | Cmd::Requeue(_)
        | Cmd::Notification(_)
        | Cmd::Alarm(_)
        | Cmd::Attention(_)
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
                Cmd::Inbox(args) => cmd_inbox(&mut c, cli, &style, args),
                Cmd::Messages(args) => cmd_messages(&mut c, cli, &style, args),
                Cmd::Requeue(args) => cmd_requeue(&mut c, cli, &style, args),
                Cmd::Notification(args) => cmd_notification(&mut c, cli, &style, args),
                Cmd::Alarm(args) => cmd_alarm(&mut c, cli, &style, args),
                Cmd::Attention(args) => cmd_attention(&mut c, cli, &style, args),
                Cmd::Hooks {
                    cmd: HooksCmd::Verify { target },
                } => hookset::run_verify(&mut c, cli.json, &style, target),
                Cmd::Hooks {
                    cmd: HooksCmd::Selftest { target },
                } => hookset::run_selftest(&mut c, cli.json, &style, target),
                Cmd::Send(_)
                | Cmd::Reply(_)
                | Cmd::Hook { .. }
                | Cmd::Name(_)
                | Cmd::Daemon { .. }
                | Cmd::Wait { .. }
                | Cmd::Hooks { .. }
                | Cmd::Ui(_)
                | Cmd::Watch { .. }
                | Cmd::Start(_)
                | Cmd::Setup { .. }
                | Cmd::Health
                | Cmd::Cleanup { .. }
                | Cmd::Theme { .. }
                | Cmd::Update { .. }
                | Cmd::Workspace { .. }
                | Cmd::Sizing { .. } => {
                    unreachable!("handled above")
                }
            }
        }
    }
}

/// cyclops watch: stream TUI by default; `--json` is the machine stream.
fn cmd_watch(cli: &Cli, style: &Style, kinds: &[String], ui: &UiArgs) -> i32 {
    // Display filters belong to the TUI. Refuse them in JSON mode instead of
    // accepting options the machine stream does not apply.
    if cli.json {
        if ui.with.is_some() || ui.from.is_some() || ui.to.is_some() {
            println!(
                "{}",
                json!({
                    "code": "unsupported_watch_filter",
                    "message": copy::WATCH_JSON_FILTER_UNSUPPORTED
                })
            );
            return EXIT_USAGE;
        }
        let mut c = match connect() {
            Ok(c) => c,
            Err(code) => return code,
        };
        return cmd_watch_json(&mut c, cli, style, kinds);
    }
    let filters = match preflight_watch_filters(ui) {
        Ok(filters) => filters,
        Err(code) => return code,
    };
    run_stream_ui(cli, ui, filters)
}

fn preflight_watch_filters(ui: &UiArgs) -> Result<cyclops_ui::Filter, i32> {
    if ui.with.is_none() && ui.from.is_none() && ui.to.is_none() {
        return Ok(cyclops_ui::Filter::default());
    }
    let mut client = connect()?;
    resolve_watch_filters(&mut client, ui)
}

/// Resolve display conveniences once to immutable endpoint identities.
/// A later rename changes only presentation and cannot strand the watch.
fn resolve_watch_filters(c: &mut Client, ui: &UiArgs) -> Result<cyclops_ui::Filter, i32> {
    let value = c
        .request(
            "status",
            serde_json::to_value(cyclops_proto::StatusParams {
                open_deliveries: false,
            })
            .expect("status params serialize"),
        )
        .map_err(|error| {
            eprintln!("{}", copy::client_error(&error, None));
            1
        })?;
    let status: StatusResult = serde_json::from_value(value).map_err(|_| {
        eprintln!("{}", copy::UNREADABLE_ANSWER);
        1
    })?;
    let unknown: Vec<&str> = [&ui.with, &ui.from, &ui.to]
        .into_iter()
        .flatten()
        .map(String::as_str)
        .filter(|asked| {
            !status.mailbox_routes.iter().any(|route| {
                route.label.as_str() == *asked || route.recipient.to_string() == *asked
            })
        })
        .collect();
    if !unknown.is_empty() {
        eprintln!("{}", copy::unknown_watch_filters(&unknown));
        return Err(EXIT_USAGE);
    }
    let resolve = |asked: &Option<String>| {
        asked.as_ref().and_then(|asked| {
            status
                .mailbox_routes
                .iter()
                .find(|route| {
                    route.label.as_str() == asked.as_str()
                        || route.recipient.to_string() == asked.as_str()
                })
                .map(|route| cyclops_ui::EndpointFilter::new(route.recipient, route.label.clone()))
        })
    };
    Ok(cyclops_ui::Filter {
        with: resolve(&ui.with),
        from: resolve(&ui.from),
        to: resolve(&ui.to),
    })
}

/// cyclops ui: deprecated alias for `cyclops watch`.
fn cmd_ui(cli: &Cli, args: &UiArgs) -> i32 {
    if cli.json {
        eprintln!("{}", copy::UI_NO_JSON);
        return EXIT_USAGE;
    }
    eprintln!("{}", copy::UI_DEPRECATED);
    let filters = match preflight_watch_filters(args) {
        Ok(filters) => filters,
        Err(code) => return code,
    };
    run_stream_ui(cli, args, filters)
}

fn run_stream_ui(cli: &Cli, args: &UiArgs, filters: cyclops_ui::Filter) -> i32 {
    cyclops_ui::run(cyclops_ui::UiOptions {
        plain: cli.plain,
        firehose: args.firehose,
        with: filters.with,
        from: filters.from,
        to: filters.to,
        backfill: args.backfill,
    })
}

/// Report compatibility facts carried by the daemon's hello.
///
/// Both mismatches warn and continue. The protocol is tolerant by design,
/// while the build identifier detects an old or shadowed daemon without
/// pretending that it cannot answer requests.
fn report_hello_mismatch(hello: &cyclops_proto::Hello) {
    if hello.proto != PROTOCOL_VERSION {
        eprintln!("{}", copy::proto_mismatch(hello.proto, PROTOCOL_VERSION));
    }
    if let Some(note) = copy::build_mismatch(hello.build.as_deref(), BUILD_REF) {
        eprintln!("{note}");
    }
}

/// Connect and check the hello. Mismatches warn once on stderr and continue.
fn connect() -> Result<Client, i32> {
    match Client::connect() {
        Ok(c) => {
            report_hello_mismatch(c.hello());
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

fn cmd_status(c: &mut Client, cli: &Cli, style: &Style) -> i32 {
    // The eye counts the record, not only the live pane fleet: open
    // deliveries carry legacy direct-delivery alarms and, folded by the
    // daemon, durable mailbox attention and held queue heads.
    let status: StatusResult = match ask(
        c,
        "status",
        json!({"open_deliveries": true}),
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
            report_hello_mismatch(client.hello());
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
    let result = match name_request(c, params, args.self_) {
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

/// A shell in a new tmux pane can run before its watcher publishes the pane.
/// Retrying only `--self` is safe because `TMUX_PANE` cannot change here and
/// only a missing target is retried.
fn name_request(c: &mut Client, params: Value, self_target: bool) -> Result<Value, ClientError> {
    let mut result = c.request("pane.label", params.clone());
    if !self_target {
        return result;
    }

    for delay in [50, 100, 200, 400, 800].map(Duration::from_millis) {
        if !matches!(
            &result,
            Err(ClientError::Server { code, .. }) if code == "no_such_target"
        ) {
            break;
        }
        std::thread::sleep(delay);
        result = c.request("pane.label", params.clone());
    }
    result
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
    let mut to: Vec<String> = Vec::new();
    if args.all {
        to.push("*".into());
    }
    for t in args.target.iter().chain(args.to.iter()) {
        if !to.contains(t) {
            to.push(t.clone());
        }
    }
    if to.is_empty() && args.reply_to.is_none() {
        eprintln!("{}", copy::NO_RECIPIENT);
        return EXIT_USAGE;
    }
    if let Err(error) = cyclops_proto::validate_message_summary(&args.summary) {
        eprintln!("{error}");
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
    let supersedes = match args.supersedes.as_deref() {
        Some(id) => match cyclops_proto::MessageId::new(id) {
            Ok(id) => Some(id),
            Err(error) => {
                eprintln!("invalid superseded message id: {error}");
                return EXIT_USAGE;
            }
        },
        None => None,
    };
    let params = serde_json::to_value(MsgSendParams {
        to: to.clone(),
        recipient_keys: None,
        expected_caller: None,
        subject: args.subject.clone(),
        summary: Some(args.summary.clone()),
        body,
        fyi: args.fyi,
        client_key: args.client_key.clone(),
        reply_to: args.reply_to.clone(),
        supersedes,
        wait: None,
        require_wake: args.require_wake,
    })
    .expect("msg.send params serialize");
    let asked = if to.len() == 1 {
        Some(to[0].as_str())
    } else {
        None
    };
    let result = match c.request("msg.send", params) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("{}", copy::client_error(&error, asked));
            return 1;
        }
    };
    if cli.json {
        println!("{result}");
        return receipts_exit_json(&result, args.require_wake);
    }
    let acceptance = match message_acceptance(&result) {
        Some(acceptance) => acceptance,
        None => {
            eprintln!("{}", copy::UNREADABLE_ANSWER);
            return 1;
        }
    };
    let receipt: MsgSendResult = match serde_json::from_value(result) {
        Ok(receipt) => receipt,
        Err(_) if !args.require_wake => {
            let verb = if acceptance.inserted.unwrap_or(true) {
                "accepted"
            } else {
                "already accepted"
            };
            println!("{verb} {}", style.accent(acceptance.msg_id.as_str()));
            println!("{}", copy::UNKNOWN_WAKE_RECEIPT);
            return 0;
        }
        Err(_) => {
            eprintln!("{}", copy::UNREADABLE_ANSWER);
            return 1;
        }
    };
    if let Some(inserted) = receipt.inserted {
        let verb = if inserted {
            "accepted"
        } else {
            "already accepted"
        };
        println!("{verb} {}", style.accent(&receipt.msg_id));
    }
    println!("{}", render::render_receipts(&receipt.deliveries, style));
    for delivery in &receipt.deliveries {
        match delivery.state {
            DeliveryState::ParkedBlockedQuota => {
                eprintln!("{}", copy::parked(&delivery.to, delivery.note.as_deref()));
            }
            DeliveryState::AttentionRequired => {
                let pane = (delivery.note.as_deref() == Some(copy::CAUSE_NO_MANIFEST))
                    .then_some(delivery.pane.as_deref())
                    .flatten();
                eprintln!(
                    "{}",
                    copy::needs_attention_for(&delivery.to, pane, delivery.note.as_deref())
                );
            }
            DeliveryState::Pasting | DeliveryState::Staged | DeliveryState::Submitted => {
                eprintln!("{}", copy::in_flight(&delivery.to));
            }
            _ => {}
        }
    }
    receipts_exit(&receipt.deliveries, args.require_wake)
}

fn cmd_inbox(c: &mut Client, cli: &Cli, style: &Style, args: &InboxArgs) -> i32 {
    match &args.cmd {
        InboxCmd::List { limit } => {
            let params = serde_json::to_value(cyclops_proto::InboxListParams {
                limit: *limit,
                sender: None,
            })
            .expect("inbox.list params serialize");
            let result: cyclops_proto::InboxListResult = match ask(
                c,
                "inbox.list",
                params,
                cli.json,
                None,
                serde_json::from_value,
            ) {
                Ok(Some(result)) => result,
                Ok(None) => return 0,
                Err(code) => return code,
            };
            for entry in result.entries {
                let subject = entry.subject.as_deref().unwrap_or("(no subject)");
                println!(
                    "{} {} · {}",
                    style.accent(entry.message_id.as_str()),
                    entry.sender_label,
                    subject
                );
            }
            0
        }
        InboxCmd::Claim { message_id } => {
            let message_id = match cyclops_proto::MessageId::new(message_id) {
                Ok(id) => id,
                Err(error) => {
                    eprintln!("{error}");
                    return EXIT_USAGE;
                }
            };
            let params = serde_json::to_value(cyclops_proto::InboxClaimParams { message_id })
                .expect("inbox.claim params serialize");
            let result: cyclops_proto::InboxClaimResult = match ask(
                c,
                "inbox.claim",
                params,
                cli.json,
                None,
                serde_json::from_value,
            ) {
                Ok(Some(result)) => result,
                Ok(None) => return 0,
                Err(code) => return code,
            };
            print_claim_payload(&result.message);
            print_skipped_oldest(result.skipped_oldest.as_ref());
            0
        }
        InboxCmd::Next { .. } => unreachable!("inbox next owns its bounded connection"),
    }
}

/// Subscribe before the first list so an arrival cannot fall between the
/// empty read and the event wait. Every subsequent read uses the same durable
/// mailbox projection as `inbox list` and the same atomic claim operation.
fn cmd_inbox_next(c: &mut Client, cli: &Cli, timeout: &str, from: Option<&str>) -> i32 {
    let budget = match parse_duration(timeout) {
        Ok(value) => value,
        Err(()) => {
            return inbox_next_failed(
                cli,
                "bad_duration",
                copy::bad_duration(timeout),
                json!({"value": timeout}),
                EXIT_USAGE,
            );
        }
    };
    let Some(deadline) = Instant::now().checked_add(budget) else {
        return inbox_next_failed(
            cli,
            "bad_duration",
            copy::bad_duration(timeout),
            json!({"value": timeout}),
            EXIT_USAGE,
        );
    };
    let sender = match from.map(str::parse::<cyclops_proto::RecipientKey>) {
        Some(Ok(sender)) => Some(sender),
        Some(Err(error)) => {
            return inbox_next_failed(
                cli,
                "invalid_recipient_key",
                error.to_string(),
                json!({"value": from}),
                EXIT_USAGE,
            );
        }
        None => None,
    };
    if let Err(code) = inbox_next_set_remaining(c, cli, budget, deadline) {
        return code;
    }
    let subscribe = serde_json::to_value(SubscribeParams {
        kinds: vec!["messages.changed".into()],
        cursor: None,
    })
    .expect("events.subscribe params serialize");
    if let Err(error) = c.subscribe(subscribe) {
        return match error {
            ClientError::ReadTimeout(_) => inbox_next_timed_out(cli, budget),
            error => inbox_next_client_failed(cli, &error),
        };
    }

    loop {
        if let Err(code) = inbox_next_set_remaining(c, cli, budget, deadline) {
            return code;
        }
        let list = match inbox_list_one(c, sender) {
            Ok(list) => list,
            Err(InboxListOneError::Client(ClientError::ReadTimeout(_))) => {
                return inbox_next_timed_out(cli, budget)
            }
            Err(InboxListOneError::Client(error)) => return inbox_next_client_failed(cli, &error),
            Err(InboxListOneError::Unreadable) => {
                return inbox_next_failed(
                    cli,
                    "unreadable_answer",
                    copy::UNREADABLE_ANSWER.to_string(),
                    Value::Null,
                    1,
                );
            }
        };
        if let Some(entry) = list.entries.into_iter().next() {
            if sender.is_some() && entry.sender != sender {
                return inbox_sender_filter_unavailable(cli);
            }
            if let Err(code) = inbox_next_set_remaining(c, cli, budget, deadline) {
                return code;
            }
            let message_id = entry.message_id;
            let params = serde_json::to_value(cyclops_proto::InboxClaimParams {
                message_id: message_id.clone(),
            })
            .expect("inbox.claim params serialize");
            let raw = match c.request("inbox.claim", params) {
                Ok(value) => value,
                Err(error) if error.certainty() == Certainty::OutcomeUnknown => {
                    return inbox_claim_outcome_unknown(cli, &message_id);
                }
                Err(ClientError::Server { code, .. }) if code == "message_not_pending" => {
                    continue;
                }
                Err(error) => return inbox_next_client_failed(cli, &error),
            };
            let result: cyclops_proto::InboxClaimResult = match serde_json::from_value(raw.clone())
            {
                Ok(result) => result,
                Err(_) => {
                    return inbox_next_failed(
                        cli,
                        "unreadable_answer",
                        copy::UNREADABLE_ANSWER.to_string(),
                        Value::Null,
                        1,
                    );
                }
            };
            if result.disposition == cyclops_proto::ClaimDisposition::Claimed {
                if cli.json {
                    println!("{raw}");
                } else {
                    print_claim_payload(&result.message);
                }
                return 0;
            }
            // Another consumer for this mailbox won the claim. Its
            // messages.changed event is already queued on this connection.
            continue;
        }

        if let Err(code) = inbox_next_set_remaining(c, cli, budget, deadline) {
            return code;
        }
        match c.next_event() {
            Ok(frame) => {
                if frame.event.event != "messages.changed" {
                    continue;
                }
            }
            Err(ClientError::ReadTimeout(_)) => {
                return inbox_next_timed_out(cli, budget);
            }
            Err(error) => return inbox_next_client_failed(cli, &error),
        }
    }
}

fn inbox_next_set_remaining(
    c: &mut Client,
    cli: &Cli,
    budget: Duration,
    deadline: Instant,
) -> Result<(), i32> {
    let Some(remaining) = inbox_next_remaining(deadline, Instant::now()) else {
        return Err(inbox_next_timed_out(cli, budget));
    };
    c.set_read_timeout(remaining);
    Ok(())
}

fn inbox_next_remaining(deadline: Instant, now: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
}

fn inbox_next_timed_out(cli: &Cli, budget: Duration) -> i32 {
    inbox_next_failed(
        cli,
        "timeout",
        copy::inbox_next_timeout(budget),
        json!({"pending": false, "waited_ms": budget.as_millis() as u64}),
        EXIT_WAIT_TIMEOUT,
    )
}

fn inbox_claim_outcome_unknown(cli: &Cli, message_id: &cyclops_proto::MessageId) -> i32 {
    inbox_next_failed(
        cli,
        "claim_outcome_unknown",
        copy::inbox_claim_outcome_unknown(message_id.as_str()),
        json!({"message_id": message_id}),
        1,
    )
}

fn inbox_sender_filter_unavailable(cli: &Cli) -> i32 {
    inbox_next_failed(
        cli,
        "sender_filter_unavailable",
        copy::INBOX_SENDER_FILTER_UNAVAILABLE.to_string(),
        Value::Null,
        1,
    )
}

fn inbox_next_failed(cli: &Cli, code: &str, message: String, data: Value, exit: i32) -> i32 {
    if cli.json {
        println!(
            "{}",
            json!({"code": code, "message": message, "data": data})
        );
    } else {
        eprintln!("{message}");
    }
    exit
}

fn inbox_next_client_failed(cli: &Cli, error: &ClientError) -> i32 {
    if !cli.json {
        return inbox_next_failed(
            cli,
            "client_error",
            copy::client_error(error, None),
            Value::Null,
            1,
        );
    }
    let (code, message, data) = inbox_next_client_error(error);
    inbox_next_failed(cli, code, message, data, 1)
}

fn inbox_next_client_error(error: &ClientError) -> (&str, String, Value) {
    match error {
        ClientError::NotRunning(_) => ("not_running", copy::client_error(error, None), Value::Null),
        ClientError::ConnectTimeout(waited) => (
            "connect_timeout",
            copy::client_error(error, None),
            json!({"waited_ms": waited.as_millis() as u64}),
        ),
        ClientError::HelloTimeout(waited) => (
            "read_timeout",
            copy::client_error(error, None),
            json!({"waited_ms": waited.as_millis() as u64}),
        ),
        ClientError::ReadTimeout(waited) => (
            "read_timeout",
            copy::client_error(error, None),
            json!({"waited_ms": waited.as_millis() as u64}),
        ),
        ClientError::RequestFrameTooLarge => (
            cyclops_proto::FrameContract::TOO_LARGE_CODE,
            copy::client_error(error, None),
            json!({"known_not_sent": true}),
        ),
        ClientError::DaemonFrameTooLarge => (
            cyclops_proto::FrameContract::TOO_LARGE_CODE,
            copy::client_error(error, None),
            json!({"known_not_sent": false}),
        ),
        ClientError::OversizedResponse(message) => (
            cyclops_proto::FrameContract::TOO_LARGE_CODE,
            message.clone(),
            Value::Null,
        ),
        ClientError::InvalidHello(_) => (
            "connection_lost",
            copy::client_error(error, None),
            Value::Null,
        ),
        ClientError::Server {
            code,
            message,
            data,
            ..
        } => (code.as_str(), message.clone(), data.clone()),
        ClientError::NotSent(_) | ClientError::Unknown(_) | ClientError::Gap(_) => (
            "connection_lost",
            copy::client_error(error, None),
            Value::Null,
        ),
    }
}

enum InboxListOneError {
    Client(ClientError),
    Unreadable,
}

fn inbox_list_one(
    c: &mut Client,
    sender: Option<cyclops_proto::RecipientKey>,
) -> Result<cyclops_proto::InboxListResult, InboxListOneError> {
    let params = serde_json::to_value(cyclops_proto::InboxListParams {
        limit: Some(1),
        sender,
    })
    .expect("inbox.list params serialize");
    let value = c
        .request("inbox.list", params)
        .map_err(InboxListOneError::Client)?;
    serde_json::from_value(value).map_err(|_| InboxListOneError::Unreadable)
}

fn cmd_messages(c: &mut Client, cli: &Cli, style: &Style, args: &MessagesArgs) -> i32 {
    let params = serde_json::to_value(cyclops_proto::MessagesSnapshotParams {
        recent_settled: args.recent_settled,
    })
    .expect("messages.snapshot params serialize");
    let snapshot: cyclops_proto::MessagesSnapshotResult = match ask(
        c,
        "messages.snapshot",
        params,
        cli.json,
        None,
        serde_json::from_value,
    ) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return 0,
        Err(code) => return code,
    };

    println!(
        "workspace {} · seq {} · {} work · {} inbox · {} outbound · {} active · {} settled · {} attention · {} shown of {}",
        snapshot.workspace_id,
        snapshot.workspace_seq,
        snapshot.counts.work_messages,
        snapshot.counts.inbox_messages,
        snapshot.counts.outbound_messages,
        snapshot.counts.active_messages,
        snapshot.counts.settled_messages,
        snapshot.counts.open_attention_entries,
        snapshot.counts.returned_messages,
        snapshot.counts.visible_messages
    );
    let heads = held_heads(&snapshot.rows);
    for line in held_queue_lines(&heads) {
        println!("{line}");
    }
    for row in &snapshot.rows {
        println!(
            "{}",
            message_snapshot_line(style.accent(row.message_id.as_str()), row, &heads)
        );
    }
    0
}

/// The head of one recipient's pending queue while it is not moving.
///
/// FIFO delivery means every later message to that recipient waits behind
/// this one. The projection already knows the id and the cause; this is the
/// index that lets every row and one summary line say so.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HeldHead {
    message_id: cyclops_proto::MessageId,
    label: String,
    cause: String,
    kind: HeldCauseKind,
    recipient: cyclops_proto::RecipientKey,
    attempt_id: Option<cyclops_proto::NotificationAttemptId>,
    /// Pending messages to the same recipient behind the head.
    waiting: usize,
}

type HeldHeads = std::collections::BTreeMap<cyclops_proto::RecipientKey, HeldHead>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeldCauseKind {
    QuotaHeld,
    QuotaResetObserved,
    BlockedPreWrite,
    Attention,
    AttentionResolutionPending,
}

/// Why a head is not moving, in the wire spelling the rest of this listing
/// uses, or `None` when the head is progressing normally.
fn held_cause(
    notification: &cyclops_proto::MessageNotificationSummary,
) -> Option<(String, HeldCauseKind)> {
    if notification.resolution.is_some() {
        return None;
    }
    match notification.quota_state {
        Some(cyclops_proto::MessageQuotaState::Held) => {
            return Some(("quota_held".to_string(), HeldCauseKind::QuotaHeld));
        }
        Some(cyclops_proto::MessageQuotaState::ResetObserved) => {
            return Some((
                "quota_reset_observed".to_string(),
                HeldCauseKind::QuotaResetObserved,
            ));
        }
        None => {}
    }
    if notification.state == cyclops_proto::MessageNotificationState::AttentionRequired {
        if notification.resolution_intent.is_some() {
            return Some((
                "attention_resolution_pending".to_string(),
                HeldCauseKind::AttentionResolutionPending,
            ));
        }
        return Some((
            notification
                .cause
                .map(|cause| wire_word(serde_json::to_value(cause).unwrap_or(Value::Null)))
                .unwrap_or_else(|| "attention_required".to_string()),
            HeldCauseKind::Attention,
        ));
    }
    if let Some(cause) = message_wake_block_reason(notification) {
        return Some((cause, HeldCauseKind::BlockedPreWrite));
    }
    None
}

fn held_heads(rows: &[cyclops_proto::MessageSnapshotRow]) -> HeldHeads {
    let mut heads = HeldHeads::new();
    for row in rows {
        for recipient in &row.recipients {
            if recipient.mailbox != cyclops_proto::MailboxEntryState::Pending
                || recipient.fifo_position != Some(1)
            {
                continue;
            }
            if let Some((cause, kind)) = held_cause(&recipient.notification) {
                heads.insert(
                    recipient.recipient,
                    HeldHead {
                        message_id: row.message_id.clone(),
                        label: recipient.label.clone(),
                        cause,
                        kind,
                        recipient: recipient.recipient,
                        attempt_id: recipient.notification.attempt_id,
                        waiting: 0,
                    },
                );
            }
        }
    }
    for row in rows {
        for recipient in &row.recipients {
            if recipient.mailbox != cyclops_proto::MailboxEntryState::Pending
                || recipient.fifo_position == Some(1)
            {
                continue;
            }
            if let Some(head) = heads.get_mut(&recipient.recipient) {
                head.waiting += 1;
            }
        }
    }
    heads
}

/// One line per held recipient queue, printed before the rows so the
/// reason a queue is not moving is the first thing on screen.
fn held_queue_lines(heads: &HeldHeads) -> Vec<String> {
    heads
        .values()
        .map(|head| {
            format!(
                "held queue · {} · head {} · {} · {} waiting · {}",
                head.label,
                head.message_id,
                head.cause,
                head.waiting,
                held_release_action(head)
            )
        })
        .collect()
}

fn held_release_action(head: &HeldHead) -> String {
    match head.kind {
        HeldCauseKind::QuotaHeld => format!(
            "next: wait for quota reset, then admin: cyclops requeue {}",
            head.message_id
        ),
        HeldCauseKind::QuotaResetObserved => {
            format!("next: admin: cyclops requeue {}", head.message_id)
        }
        HeldCauseKind::BlockedPreWrite => {
            let mut action = format!(
                "next: fix {}; or recipient retrieves the durable payload with cyclops inbox claim {}",
                head.cause, head.message_id
            );
            if let Some(attempt_id) = head.attempt_id {
                action.push_str(&format!(
                    "; or admin: cyclops notification withdraw {attempt_id} --recipient {}",
                    head.recipient
                ));
            }
            action
        }
        HeldCauseKind::Attention => match head.attempt_id {
            Some(attempt_id) => format!(
                "next: recipient retrieves the durable payload with cyclops inbox claim {}; or admin: cyclops attention show {attempt_id} --diff, then complete or discard when its checks authorize the action",
                head.message_id
            ),
            None => format!(
                "next: recipient retrieves the durable payload with cyclops inbox claim {}",
                head.message_id
            ),
        },
        HeldCauseKind::AttentionResolutionPending => match head.attempt_id {
            Some(attempt_id) => format!(
                "next: recipient retrieves the durable payload with cyclops inbox claim {}; or admin inspects cyclops attention show {attempt_id} --diff; do not repeat a terminal action",
                head.message_id
            ),
            None => format!(
                "next: recipient retrieves the durable payload with cyclops inbox claim {}; do not repeat a terminal action",
                head.message_id
            ),
        },
    }
}

fn message_snapshot_line(
    styled_id: String,
    row: &cyclops_proto::MessageSnapshotRow,
    heads: &HeldHeads,
) -> String {
    let direction = match row.direction {
        cyclops_proto::MessageDirection::Inbound => "inbound",
        cyclops_proto::MessageDirection::Outbound => "outbound",
        cyclops_proto::MessageDirection::SelfAddressed => "self addressed",
        cyclops_proto::MessageDirection::Workspace => "workspace",
    };
    let work = if row.needs_action { " · work" } else { "" };
    let recipients = row
        .recipients
        .iter()
        .map(|recipient| {
            message_recipient_cell(&row.message_id, recipient, heads.get(&recipient.recipient))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let subject = row.subject.as_deref().unwrap_or("(no subject)");
    format!(
        "{styled_id} {direction}{work} · {} -> {recipients} · {subject} · thread {}",
        row.sender_label, row.thread_message_count
    )
}

fn message_recipient_cell(
    message_id: &cyclops_proto::MessageId,
    recipient: &cyclops_proto::MessageRecipientSummary,
    held: Option<&HeldHead>,
) -> String {
    let mailbox = match &recipient.mailbox {
        cyclops_proto::MailboxEntryState::Pending => match recipient.fifo_position {
            Some(1) => "pending · oldest".to_string(),
            Some(position) => {
                let mut cell = format!("pending · {} ahead", position.saturating_sub(1));
                if let Some(head) = held {
                    cell.push_str(&format!(" · behind {} ({})", head.message_id, head.cause));
                }
                cell
            }
            None => "pending".to_string(),
        },
        cyclops_proto::MailboxEntryState::Claimed { .. } => "claimed".to_string(),
        cyclops_proto::MailboxEntryState::DeliveredDirect { .. } => {
            "delivered directly".to_string()
        }
        cyclops_proto::MailboxEntryState::Superseded { .. } => "superseded".to_string(),
    };
    let mut includes_attempt = false;
    let mut notification = if recipient.notification.operator_withdrawn == Some(true) {
        "wake withdrawn by admin · message remains claimable".to_string()
    } else if recipient.notification.settlement
        == Some(cyclops_proto::MessageNotificationSettlement::WithdrawnByClaim)
    {
        "withdrawn".to_string()
    } else {
        match recipient.notification.resolution {
            Some(cyclops_proto::NotificationResolution::Complete) => "wake submitted".to_string(),
            Some(cyclops_proto::NotificationResolution::Discard) => "wake discarded".to_string(),
            None => match recipient.notification.quota_state {
                Some(cyclops_proto::MessageQuotaState::Held) => {
                    "quota held · wait for quota reset · no automatic resume".to_string()
                }
                Some(cyclops_proto::MessageQuotaState::ResetObserved) => format!(
                "quota reset observed · admin next: cyclops requeue {message_id} · message wide"
            ),
                None => match message_wake_block_reason(&recipient.notification) {
                    Some(reason) => {
                        let mut blocked = format!("wake blocked before write: {reason}");
                        if let Some(updated_at) = recipient.notification.updated_at {
                            blocked.push_str(&format!(
                                " · waited {}",
                                render::human_duration(render::now_ms().saturating_sub(updated_at))
                            ));
                        }
                        blocked.push_str(" · ");
                        blocked.push_str(
                            &recipient
                                .current_route
                                .as_ref()
                                .map(|route| {
                                    format!("current route {} ({})", route.label, route.pane_id)
                                })
                                .unwrap_or_else(|| "route unavailable".into()),
                        );
                        if recipient.can_withdraw_notification {
                            if let Some(attempt_id) = recipient.notification.attempt_id {
                                blocked.push_str(&format!(
                                    " · admin next: cyclops notification withdraw {attempt_id} --recipient {}",
                                    recipient.recipient
                                ));
                                includes_attempt = true;
                            }
                        } else if recipient.direction == cyclops_proto::MessageDirection::Inbound
                            && matches!(
                                recipient.mailbox,
                                cyclops_proto::MailboxEntryState::Pending
                            )
                        {
                            blocked.push_str(&format!(" · next: cyclops inbox claim {message_id}"));
                        }
                        blocked
                    }
                    None => match recipient.notification.state {
                        cyclops_proto::MessageNotificationState::NotStarted => {
                            "not started".to_string()
                        }
                        cyclops_proto::MessageNotificationState::Gating => {
                            "checking readiness".to_string()
                        }
                        cyclops_proto::MessageNotificationState::AttentionRequired => {
                            "needs attention".to_string()
                        }
                        state => wire_word(serde_json::to_value(state).unwrap_or(Value::Null)),
                    },
                },
            },
        }
    };
    if recipient.notification.resolution.is_none() {
        if let Some(intent) = recipient.notification.resolution_intent {
            notification = match recipient.notification.resolution_action_accepted {
                Some(accepted) if accepted == intent => {
                    if intent == cyclops_proto::NotificationResolution::Complete
                        && recipient
                            .notification
                            .resolution_consumption_observed
                            .is_none()
                    {
                        "terminal accepted, task start unproven; no retry or reconciliation available"
                            .to_string()
                    } else {
                        match recipient.notification.attempt_id {
                            Some(attempt_id) => {
                                includes_attempt = true;
                                format!(
                                    "terminal accepted the action key; {}",
                                    copy::attention_action_uncertain(intent, attempt_id)
                                )
                            }
                            None => {
                                "terminal accepted the action key; exact attempt unavailable for reconciliation"
                                    .to_string()
                            }
                        }
                    }
                }
                None => {
                    let action = match intent {
                        cyclops_proto::NotificationResolution::Complete => "submit",
                        cyclops_proto::NotificationResolution::Discard => "discard",
                    };
                    format!(
                        "{action} intent recorded; terminal acceptance unproven; no retry or reconciliation available"
                    )
                }
                Some(_) => "terminal action records disagree; no retry or reconciliation available"
                    .to_string(),
            };
        } else if recipient.notification.resolution_action_accepted.is_some() {
            notification =
                "terminal acceptance recorded without a matching intent; no retry or reconciliation available"
                    .to_string();
        } else if recipient
            .notification
            .resolution_consumption_observed
            .is_some()
        {
            notification =
                "task start evidence recorded without matching terminal action facts; no retry or reconciliation available"
                    .to_string();
        }
        if let Some(cause) = recipient.notification.cause {
            notification.push(':');
            notification.push_str(&wire_word(
                serde_json::to_value(cause).unwrap_or(Value::Null),
            ));
        }
        if let Some(outcome) = recipient.notification.verify_outcome {
            notification.push_str(":verify=");
            notification.push_str(&wire_word(
                serde_json::to_value(outcome.kind).unwrap_or(Value::Null),
            ));
            notification.push('/');
            notification.push_str(&wire_word(
                serde_json::to_value(outcome.observed_composer).unwrap_or(Value::Null),
            ));
        }
        if let Some(cleared) = recipient.notification.attention_cleared {
            notification.push(':');
            notification.push_str(if cleared { "cleared" } else { "open" });
        }
    }
    if !includes_attempt {
        if let Some(attempt_id) = recipient.notification.attempt_id {
            notification.push(' ');
            notification.push_str(&attempt_id.to_string());
        }
    }
    let availability = if recipient.available {
        ""
    } else {
        "; unavailable"
    };
    format!(
        "{} [{mailbox}; {notification}{availability}]",
        recipient.label
    )
}

fn message_wake_block_reason(
    notification: &cyclops_proto::MessageNotificationSummary,
) -> Option<String> {
    notification
        .pane_width_block()
        .map(|(observed, required)| copy::pane_too_narrow(observed, required))
        .or_else(|| {
            notification
                .wake_block
                .map(|block| block.wire_name().to_string())
        })
        // The named block (for example hook_admission_unproven) is more
        // exact than the enum cause it was recorded under.
        .or_else(|| notification.pre_write_block.clone())
        .or_else(|| {
            notification
                .pre_write_cause
                .map(|cause| cause.wire_name().to_string())
        })
}

/// F5: a claim by id that jumped the queue says what it left at the head,
/// so a recipient cannot keep skipping a stuck oldest message unknowingly.
fn print_skipped_oldest(skipped: Option<&cyclops_proto::MessageId>) {
    if let Some(oldest) = skipped {
        println!("{}", copy::claim_skipped_oldest(oldest.as_str()));
    }
}

fn print_claim_payload(message: &cyclops_proto::InboxMessage) {
    let subject = message.subject.as_deref().unwrap_or("(no subject)");
    if let Some(recipient_label) = &message.recipient_label {
        println!(
            "[cyclops {}] TO: {}  FROM: {}  SUBJECT: {}",
            message.message_id, recipient_label, message.sender_label, subject
        );
    } else {
        // Older daemons do not carry the immutable recipient presentation.
        println!(
            "[cyclops {}] FROM: {}  SUBJECT: {}",
            message.message_id, message.sender_label, subject
        );
    }
    if let Some(summary) = &message.summary {
        println!("Summary: {summary}");
    }
    if let Some(body) = &message.body {
        println!("{body}");
    }
    if message.kind != cyclops_proto::Kind::Fyi {
        println!(
            "Reply: cyclops reply {} --summary \"First sentence. Second sentence.\" --body \"...\"",
            message.message_id
        );
    }
    println!("[cyclops:end {}]", message.message_id);
}

fn cmd_reply(cli: &Cli, style: &Style, args: &ReplyArgs) -> i32 {
    let message_id = match cyclops_proto::MessageId::new(&args.message_id) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("{error}");
            return EXIT_USAGE;
        }
    };
    if let Err(error) = cyclops_proto::validate_message_summary(&args.summary) {
        eprintln!("{error}");
        return EXIT_USAGE;
    }
    let body = match (&args.body, &args.body_file) {
        (Some(body), _) => body.clone(),
        (None, Some(path)) => match read_body_file(path) {
            Ok(body) => body,
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
    let params = serde_json::to_value(cyclops_proto::ReplyParams {
        message_id,
        summary: Some(args.summary.clone()),
        body,
        client_key: args.client_key.clone(),
    })
    .expect("msg.reply params serialize");
    let result = match c.request("msg.reply", params) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("{}", copy::client_error(&error, None));
            return 1;
        }
    };
    if cli.json {
        println!("{result}");
        return i32::from(message_acceptance(&result).is_none());
    }
    let acceptance = match message_acceptance(&result) {
        Some(acceptance) => acceptance,
        None => {
            eprintln!("{}", copy::UNREADABLE_ANSWER);
            return 1;
        }
    };
    let result: MsgSendResult = match serde_json::from_value(result) {
        Ok(result) => result,
        Err(_) => {
            let verb = if acceptance.inserted.unwrap_or(true) {
                "accepted"
            } else {
                "already accepted"
            };
            println!("{verb} {}", style.accent(acceptance.msg_id.as_str()));
            println!("{}", copy::UNKNOWN_WAKE_RECEIPT);
            return 0;
        }
    };
    let verb = if result.inserted.unwrap_or(true) {
        "accepted"
    } else {
        "already accepted"
    };
    println!("{verb} {}", style.accent(&result.msg_id));
    0
}

fn cmd_requeue(c: &mut Client, cli: &Cli, style: &Style, args: &RequeueArgs) -> i32 {
    let message_id = match cyclops_proto::MessageId::new(&args.message_id) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("{error}");
            return EXIT_USAGE;
        }
    };
    let params = serde_json::to_value(cyclops_proto::RequeueParams { message_id })
        .expect("msg.requeue params serialize");
    let result: cyclops_proto::RequeueResult = match ask(
        c,
        "msg.requeue",
        params,
        cli.json,
        None,
        serde_json::from_value,
    ) {
        Ok(Some(result)) => result,
        Ok(None) => return 0,
        Err(code) => return code,
    };
    if result.requeued {
        println!("requeued {}", style.accent(result.message_id.as_str()));
        0
    } else {
        eprintln!("message is not requeueable");
        1
    }
}

fn cmd_notification(c: &mut Client, cli: &Cli, style: &Style, args: &NotificationArgs) -> i32 {
    match &args.cmd {
        NotificationCmd::Withdraw {
            attempt_id,
            recipient,
        } => {
            let params = serde_json::to_value(cyclops_proto::NotificationWithdrawParams {
                attempt_id: *attempt_id,
                recipient: *recipient,
            })
            .expect("notification.withdraw params serialize");
            let result: cyclops_proto::NotificationWithdrawResult = match ask(
                c,
                "notification.withdraw",
                params,
                cli.json,
                None,
                serde_json::from_value,
            ) {
                Ok(Some(result)) => result,
                Ok(None) => return 0,
                Err(code) => return code,
            };
            let verb = match result.disposition {
                cyclops_proto::NotificationWithdrawDisposition::Withdrawn => "withdrew",
                cyclops_proto::NotificationWithdrawDisposition::AlreadyWithdrawn => {
                    "already withdrew"
                }
            };
            println!(
                "{verb} {} for {}; message {} remains claimable",
                style.accent(&result.attempt_id.to_string()),
                result.recipient,
                style.accent(result.message_id.as_str())
            );
            0
        }
    }
}

/// The wire spelling of one serializable value, for display.
///
/// Printing what the daemon sent keeps the shown word and the JSON field
/// identical. A separate Display impl would be a second spelling to keep
/// in step with the protocol.
/// One preview line: who, which message, and why it needs attention.
///
/// Built as a value so the shape is testable. The identifier arrives
/// already styled because colour is the caller's business, not this
/// function's.
fn alarm_line(styled_id: &str, alarm: &cyclops_proto::AlarmSummary) -> String {
    format!(
        "{} {} · {} · {} · {}",
        styled_id,
        alarm.recipient,
        alarm.message_id,
        wire_word(serde_json::to_value(alarm.state).unwrap_or(Value::Null)),
        wire_word(serde_json::to_value(alarm.cause).unwrap_or(Value::Null))
    )
}

fn wire_word(value: Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| "unknown".to_string())
}

/// Ask once for the exact age-selected set. The caller either renders it
/// as a preview or freezes its ids before asking the operator to clear.
fn alarm_preview(
    c: &mut Client,
    cli: &Cli,
    older_than: &str,
) -> Result<Option<cyclops_proto::AlarmPreviewResult>, i32> {
    let older_than_ms = match parse_wire_duration_ms(older_than) {
        Ok(age) => age,
        Err(()) => {
            eprintln!("{}", copy::bad_duration(older_than));
            return Err(EXIT_USAGE);
        }
    };
    let params = serde_json::to_value(cyclops_proto::AlarmPreviewParams { older_than_ms })
        .expect("alarm.preview params serialize");
    ask(
        c,
        "alarm.preview",
        params,
        cli.json,
        None,
        serde_json::from_value,
    )
}

/// Confirmation is exact and deliberately stronger than a generic yes.
/// The prompt names the frozen selection's count and age cutoff.
fn confirm_age_clear<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    count: usize,
    older_than: &str,
) -> std::io::Result<bool> {
    write!(
        output,
        "{}",
        copy::alarm_clear_confirmation(count, older_than)
    )?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    Ok(answer.trim() == "clear")
}

/// The line printed under `cleared <id>`: an acknowledgement changes no
/// state, so say what the daemon observed under the clearance lock.
fn alarm_cleared_consequence(alarm: &cyclops_proto::AlarmSummary) -> String {
    copy::alarm_cleared_consequence(
        &alarm.id,
        &alarm.message_id,
        &alarm.recipient,
        &wire_word(serde_json::to_value(alarm.state).unwrap_or(Value::Null)),
        &wire_word(serde_json::to_value(alarm.cause).unwrap_or(Value::Null)),
    )
}

fn clear_alarm_ids(
    c: &mut Client,
    cli: &Cli,
    style: &Style,
    ids: Vec<String>,
    cutoff_ms: Option<u64>,
) -> i32 {
    let params = serde_json::to_value(cyclops_proto::AlarmClearParams { ids, cutoff_ms })
        .expect("alarm.clear params serialize");
    let result: cyclops_proto::AlarmClearResult = match ask(
        c,
        "alarm.clear",
        params,
        cli.json,
        None,
        serde_json::from_value,
    ) {
        Ok(Some(result)) => result,
        Ok(None) => return 0,
        Err(code) => return code,
    };
    let summaries: std::collections::BTreeMap<_, _> = result
        .summaries
        .into_iter()
        .map(|summary| (summary.id.clone(), summary))
        .collect();
    for id in result.cleared_ids {
        println!("cleared {}", style.accent(&id));
        if let Some(summary) = summaries.get(&id) {
            println!("{}", alarm_cleared_consequence(summary));
        } else {
            println!("{}", copy::alarm_cleared_without_summary(&id));
        }
    }
    0
}

fn cmd_alarm(c: &mut Client, cli: &Cli, style: &Style, args: &AlarmArgs) -> i32 {
    match &args.cmd {
        AlarmCmd::Preview { older_than } => {
            let result = match alarm_preview(c, cli, older_than) {
                Ok(Some(result)) => result,
                Ok(None) => return 0,
                Err(code) => return code,
            };
            for alarm in result.entries {
                println!("{}", alarm_line(&style.accent(&alarm.id), &alarm));
            }
            0
        }
        AlarmCmd::Clear { ids, older_than } => {
            let Some(older_than) = older_than else {
                return clear_alarm_ids(c, cli, style, ids.clone(), None);
            };
            if cli.json {
                eprintln!("{}", copy::ALARM_CLEAR_JSON_REQUIRES_CONFIRMATION);
                return EXIT_USAGE;
            }
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                eprintln!("{}", copy::ALARM_CLEAR_TERMINAL_REQUIRED);
                return EXIT_USAGE;
            }
            let result = match alarm_preview(c, cli, older_than) {
                Ok(Some(result)) => result,
                Ok(None) => unreachable!("interactive age clear never uses JSON output"),
                Err(code) => return code,
            };
            if result.entries.is_empty() {
                println!("{}", copy::no_unresolved_alarms(older_than));
                return 0;
            }
            for alarm in &result.entries {
                println!("{}", alarm_line(&style.accent(&alarm.id), alarm));
            }
            let cutoff_ms = result.cutoff_ms;
            let ids: Vec<String> = result.entries.into_iter().map(|alarm| alarm.id).collect();
            let confirmed = confirm_age_clear(
                &mut std::io::stdin().lock(),
                &mut std::io::stdout().lock(),
                ids.len(),
                older_than,
            );
            match confirmed {
                Ok(true) => clear_alarm_ids(c, cli, style, ids, Some(cutoff_ms)),
                Ok(false) => {
                    println!("{}", copy::ALARM_CLEARANCE_CANCELLED);
                    0
                }
                Err(error) => {
                    eprintln!("{}", copy::alarm_clear_confirmation_unreadable(&error));
                    1
                }
            }
        }
    }
}

fn cmd_attention(c: &mut Client, cli: &Cli, style: &Style, args: &AttentionArgs) -> i32 {
    match &args.cmd {
        AttentionCmd::Show { id, diff } => {
            let params = serde_json::to_value(cyclops_proto::AttentionShowParams {
                id: id.clone(),
                diff: *diff,
            })
            .expect("attention.show params serialize");
            let result: cyclops_proto::AttentionShowResult = match ask(
                c,
                "attention.show",
                params,
                cli.json,
                None,
                serde_json::from_value,
            ) {
                Ok(Some(result)) => result,
                Ok(None) => return 0,
                Err(code) => return code,
            };
            print_attention_checks(style, &result);
            if *diff {
                match (result.expected.as_deref(), result.observed.as_deref()) {
                    (Some(expected), Some(observed)) => {
                        print!("{}", local_line_diff(expected, observed))
                    }
                    _ => println!("{}", copy::ATTENTION_DIFF_UNAVAILABLE),
                }
            }
            0
        }
        AttentionCmd::Complete { id } => resolve_attention(c, cli, style, id, "attention.complete"),
        AttentionCmd::Discard { id } => resolve_attention(c, cli, style, id, "attention.discard"),
    }
}

fn resolve_attention(c: &mut Client, cli: &Cli, style: &Style, id: &str, method: &str) -> i32 {
    let params = serde_json::to_value(cyclops_proto::AttentionResolveParams { id: id.to_string() })
        .expect("attention resolution params serialize");
    let result: cyclops_proto::AttentionResolveResult =
        match ask(c, method, params, cli.json, None, serde_json::from_value) {
            Ok(Some(result)) => result,
            Ok(None) => return 0,
            Err(code) => return code,
        };
    let verb = copy::attention_resolution_verb(result.resolution);
    println!("{verb} {}", style.accent(&result.attempt_id.to_string()));
    0
}

fn print_attention_checks(style: &Style, result: &cyclops_proto::AttentionShowResult) {
    println!(
        "{} · {} · {}",
        style.accent(&result.attempt_id.to_string()),
        result.recipient,
        result.message_id
    );
    for (name, passed) in copy::attention_check_rows(&result.checks) {
        println!("  {name}: {}", copy::attention_check_value(passed));
    }
    if let Some(line) = attention_verify_failure_line(result.verify_outcome) {
        println!("  {line}");
    }
}

fn attention_verify_failure_line(
    outcome: Option<cyclops_proto::NotificationVerifyOutcome>,
) -> Option<String> {
    outcome.map(|outcome| {
        let kind = wire_word(serde_json::to_value(outcome.kind).unwrap_or(Value::Null));
        let composer =
            wire_word(serde_json::to_value(outcome.observed_composer).unwrap_or(Value::Null));
        format!("verification failure: {kind} · composer {composer}")
    })
}

/// Compact line diff computed by the client. The daemon never receives it.
fn local_line_diff(expected: &str, observed: &str) -> String {
    let expected: Vec<_> = expected.split('\n').collect();
    let observed: Vec<_> = observed.split('\n').collect();
    let mut prefix = 0;
    while prefix < expected.len() && prefix < observed.len() && expected[prefix] == observed[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < expected.len().saturating_sub(prefix)
        && suffix < observed.len().saturating_sub(prefix)
        && expected[expected.len() - 1 - suffix] == observed[observed.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let mut out = String::from("--- expected\n+++ composer\n");
    let context_start = prefix.saturating_sub(2);
    for line in &expected[context_start..prefix] {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    for line in &expected[prefix..expected.len().saturating_sub(suffix)] {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
    for line in &observed[prefix..observed.len().saturating_sub(suffix)] {
        out.push_str("+ ");
        out.push_str(line);
        out.push('\n');
    }
    let suffix_start = expected.len().saturating_sub(suffix);
    for line in expected.iter().skip(suffix_start).take(2) {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// cyclops wait <target> --until idle|turn-ended|blocked [--timeout 60s].
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
        "until": until.wire_word(),
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
                    copy::wait_timeout(target, until.human_word(), budget, data["state"].as_str(),)
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
        read_bounded_body(std::io::stdin())
    } else {
        let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        read_bounded_body(file)
    }
}

fn read_bounded_body(reader: impl std::io::Read) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .take((cyclops_proto::FrameContract::MAX_JSON_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > cyclops_proto::FrameContract::MAX_JSON_BYTES {
        return Err(format!(
            "{}; nothing was sent",
            copy::frame_too_large("message body")
        ));
    }
    String::from_utf8(bytes).map_err(|_| "message body is not UTF-8".to_string())
}

struct MessageAcceptance {
    msg_id: cyclops_proto::MessageId,
    inserted: Option<bool>,
}

/// Decode only the fields that prove the daemon accepted a durable message.
/// Delivery receipts remain a separate compatibility boundary.
fn message_acceptance(value: &Value) -> Option<MessageAcceptance> {
    let object = value.as_object()?;
    let msg_id = cyclops_proto::MessageId::new(object.get("msg_id")?.as_str()?).ok()?;
    if object.get("seq")?.as_u64()? == 0 {
        return None;
    }
    object.get("deliveries")?.as_array()?;
    let inserted = match object.get("inserted") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(inserted)) => Some(*inserted),
        Some(_) => return None,
    };
    Some(MessageAcceptance { msg_id, inserted })
}

/// Durable acceptance is the default success contract. Scripts that also
/// require the optional pane wake ask for that stronger contract explicitly.
fn receipts_exit(ds: &[DeliveryReceipt], require_wake: bool) -> i32 {
    i32::from(
        require_wake && (ds.is_empty() || ds.iter().any(|receipt| !receipt_proves_wake(receipt))),
    )
}

fn receipt_proves_wake(receipt: &DeliveryReceipt) -> bool {
    if receipt.pre_write_cause.is_some()
        || receipt.wake_block.is_some()
        || delivery_needs_human(receipt.state)
    {
        return false;
    }

    match receipt.notification_state {
        Some(MessageNotificationState::Submitted | MessageNotificationState::Notified) => true,
        Some(_) => false,
        None => matches!(
            receipt.state,
            DeliveryState::Submitted
                | DeliveryState::DeliveredVerified
                | DeliveryState::DeliveredUnverified
        ),
    }
}

/// JSON output remains an untouched daemon response. Strong wake evaluation
/// decodes that response into the same receipt type as plain output and fails
/// closed when a required field or state is not understood.
fn receipts_exit_json(v: &Value, require_wake: bool) -> i32 {
    if message_acceptance(v).is_none() {
        return 1;
    }
    if !require_wake {
        return 0;
    }
    serde_json::from_value::<MsgSendResult>(v.clone())
        .map(|result| receipts_exit(&result.deliveries, true))
        .unwrap_or(1)
}

fn cmd_watch_json(c: &mut Client, cli: &Cli, style: &Style, kinds: &[String]) -> i32 {
    let params = serde_json::to_value(SubscribeParams {
        kinds: kinds.to_vec(),
        cursor: None,
    })
    .expect("events.subscribe params serialize");
    if let Err(e) = c.subscribe(params) {
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
        let frame = match c.next_event() {
            Ok(frame) => frame,
            Err(e) => {
                eprintln!("{}", copy::client_error(&e, None));
                return 1;
            }
        };
        if cli.json {
            let _ = writeln!(stdout, "{}", frame.raw_text());
        } else {
            let _ = writeln!(
                stdout,
                "{}",
                render::render_event_line(&frame.event, style, render::now_ms())
            );
        }
        let _ = stdout.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_next_oversized_response_keeps_the_existing_json_shape() {
        // Obsolete when inbox-next's documented machine error schema gains a
        // typed uncertainty field for oversized daemon responses.
        let message = "daemon response was too large; request outcome is unknown";
        let error = ClientError::OversizedResponse(message.into());
        let (code, rendered, data) = inbox_next_client_error(&error);
        assert_eq!(code, cyclops_proto::FrameContract::TOO_LARGE_CODE);
        assert_eq!(rendered, message);
        assert_eq!(data, Value::Null);
    }

    #[test]
    fn body_inputs_are_bounded_before_request_serialization() {
        let exact = vec![b'x'; cyclops_proto::FrameContract::MAX_JSON_BYTES];
        assert_eq!(
            read_bounded_body(std::io::Cursor::new(exact))
                .expect("the body boundary is readable")
                .len(),
            cyclops_proto::FrameContract::MAX_JSON_BYTES
        );

        let oversized = vec![b'x'; cyclops_proto::FrameContract::MAX_JSON_BYTES + 1];
        let error = read_bounded_body(std::io::Cursor::new(oversized))
            .expect_err("an oversized body must stop before request serialization");
        assert!(error.contains("nothing was sent"), "{error}");
    }

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
        for bad in [
            "",
            "0",
            "0s",
            "nope",
            "10x",
            "s",
            "-5s",
            "1.5s",
            "5s junk",
            "18446744073709551615m",
        ] {
            assert_eq!(parse_duration(bad), Err(()), "{bad:?} should not parse");
        }
        assert_eq!(parse_duration("18446744073709551615m"), Err(()));
        assert_eq!(parse_duration("18446744073709551615s1s"), Err(()));
        assert_eq!(parse_wire_duration_ms("18446744073709551615"), Err(()));
    }

    #[test]
    fn health_is_a_daemon_independent_top_level_command() {
        let parsed = Cli::try_parse_from(["cyclops", "--json", "health"]).unwrap();
        assert!(parsed.json);
        assert!(matches!(parsed.cmd, Some(Cmd::Health)));
    }

    #[test]
    fn cleanup_is_dry_run_by_default_and_names_only_closed_asset_classes() {
        let parsed = Cli::try_parse_from([
            "cyclops",
            "--json",
            "cleanup",
            "build-cache",
            "update-scratch",
        ])
        .unwrap();
        assert!(parsed.json);
        assert!(matches!(
            parsed.cmd,
            Some(Cmd::Cleanup {
                assets,
                apply: false,
            }) if assets == [cleanup::AssetClass::BuildCache, cleanup::AssetClass::UpdateScratch]
        ));
        assert!(Cli::try_parse_from(["cyclops", "cleanup", "/tmp"]).is_err());
        assert!(Cli::try_parse_from(["cyclops", "cleanup"]).is_err());
    }

    #[test]
    fn inbox_next_recomputes_one_budget_before_each_socket_operation() {
        let start = Instant::now();
        let deadline = start.checked_add(Duration::from_millis(200)).unwrap();
        assert_eq!(
            inbox_next_remaining(deadline, start + Duration::from_millis(120)),
            Some(Duration::from_millis(80))
        );
        assert_eq!(inbox_next_remaining(deadline, deadline), None);
        assert_eq!(
            inbox_next_remaining(deadline, deadline + Duration::from_millis(1)),
            None
        );
    }

    #[test]
    fn inbox_claim_requires_one_message_shaped_target() {
        assert!(Cli::try_parse_from(["cyclops", "inbox", "claim"]).is_err());
        assert!(Cli::try_parse_from(["cyclops", "inbox", "claim", "m-1"]).is_ok());
        assert!(Cli::try_parse_from(
            ["cyclops", "inbox", "claim", "m-att_--AAAAAAQACAAAAAAAAAAQ",]
        )
        .is_ok());
    }

    #[test]
    fn send_keeps_reply_and_supersession_flags() {
        let reply = Cli::try_parse_from([
            "cyclops",
            "send",
            "--subject",
            "ignored for validated reply",
            "--summary",
            "This replies to the parent. The route stays exact.",
            "--reply-to",
            "m-parent",
            "--client-key",
            "retry-1",
        ])
        .unwrap();
        let Some(Cmd::Send(args)) = reply.cmd else {
            panic!("send command")
        };
        assert_eq!(args.reply_to.as_deref(), Some("m-parent"));
        assert_eq!(args.client_key.as_deref(), Some("retry-1"));
        assert!(args.target.is_none());
        assert!(Cli::try_parse_from([
            "cyclops",
            "send",
            "reviewer",
            "--subject",
            "ignored",
            "--summary",
            "This is invalid here. The recipient selector conflicts.",
            "--reply-to",
            "m-parent",
        ])
        .is_err());

        let supersession = Cli::try_parse_from([
            "cyclops",
            "send",
            "reviewer",
            "--subject",
            "replacement",
            "--summary",
            "This replaces the prior handoff. The new facts are current.",
            "--supersedes",
            "m-old",
        ])
        .unwrap();
        let Some(Cmd::Send(args)) = supersession.cmd else {
            panic!("send command")
        };
        assert_eq!(args.supersedes.as_deref(), Some("m-old"));
        assert!(args.reply_to.is_none());
    }

    #[test]
    fn send_rejects_the_removed_wait_option() {
        assert!(Cli::try_parse_from([
            "cyclops",
            "send",
            "reviewer",
            "--subject",
            "run tests",
            "--summary",
            "Run the focused tests. Report the exact result.",
            "--wait",
            "turn-ended",
        ])
        .is_err());
    }

    #[test]
    fn send_and_reply_require_a_summary_argument() {
        assert!(
            Cli::try_parse_from(["cyclops", "send", "reviewer", "--subject", "Review",]).is_err()
        );
        assert!(Cli::try_parse_from([
            "cyclops",
            "send",
            "reviewer",
            "--subject",
            "Review",
            "--summary",
            "Review the patch. Report any blocker.",
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["cyclops", "reply", "m-parent"]).is_err());
        assert!(Cli::try_parse_from([
            "cyclops",
            "reply",
            "m-parent",
            "--summary",
            "The review is complete. No blockers remain.",
        ])
        .is_ok());
    }

    #[test]
    fn send_help_does_not_advertise_removed_wait_options() {
        let error = match Cli::try_parse_from(["cyclops", "send", "--help"]) {
            Ok(_) => panic!("send --help returned a command instead of help"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = error.to_string();
        assert!(!help.contains("--wait"), "{help}");
        assert!(!help.contains("--timeout"), "{help}");
    }

    #[test]
    fn send_help_states_the_bounded_require_wake_contract() {
        let error = match Cli::try_parse_from(["cyclops", "send", "--help"]) {
            Ok(_) => panic!("send --help returned a command instead of help"),
            Err(error) => error,
        };
        let help = error.to_string();
        for words in [
            "every recipient's bounded receipt",
            "submitted or notified",
            "legacy direct-delivery",
            "waits past writing and staging",
            "never for agent work or message completion",
        ] {
            assert!(help.contains(words), "missing {words:?} in {help}");
        }
    }

    #[test]
    fn wait_help_names_the_observed_transition_without_done() {
        let error = match Cli::try_parse_from(["cyclops", "wait", "--help"]) {
            Ok(_) => panic!("wait --help returned a command instead of help"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = error.to_string();
        assert!(help.contains("working was observed"), "{help}");
        assert!(help.contains("idle_with_input"), "{help}");
        assert!(help.contains("no turn or message identity"), "{help}");
        assert!(help.contains("turn-ended"), "{help}");
        assert!(!help.contains("done"), "{help}");
        assert!(!help.contains("turn ends"), "{help}");

        assert!(
            Cli::try_parse_from(["cyclops", "wait", "reviewer", "--until", "turn-ended",]).is_ok()
        );
        assert!(Cli::try_parse_from(["cyclops", "wait", "reviewer", "--until", "done"]).is_err());
    }

    #[test]
    fn alarm_clear_requires_explicit_identifiers() {
        assert!(Cli::try_parse_from(["cyclops", "alarm", "clear"]).is_err());
        assert!(Cli::try_parse_from(["cyclops", "alarm", "clear", "a-1"]).is_ok());
        assert!(Cli::try_parse_from(["cyclops", "alarm", "clear", "--older-than", "30m"]).is_ok());
        assert!(
            Cli::try_parse_from(["cyclops", "alarm", "clear", "a-1", "--older-than", "30m"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["cyclops", "alarm", "clear", "--all"]).is_err());
    }

    #[test]
    fn age_clear_confirmation_names_the_frozen_selection() {
        let mut input = std::io::Cursor::new(b"clear\n");
        let mut output = Vec::new();
        assert!(confirm_age_clear(&mut input, &mut output, 3, "30m").unwrap());
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Clear 3 alarms selected by --older-than 30m? Type clear to confirm: "
        );

        for answer in [b"yes\n".as_slice(), b"CLEAR\n", b"\n"] {
            let mut input = std::io::Cursor::new(answer);
            assert!(!confirm_age_clear(&mut input, &mut Vec::new(), 3, "30m").unwrap());
        }
    }

    #[test]
    fn attention_commands_require_one_explicit_target() {
        assert!(Cli::try_parse_from(["cyclops", "attention", "show"]).is_err());
        let shown =
            Cli::try_parse_from(["cyclops", "attention", "show", "att-1", "--diff"]).unwrap();
        let Some(Cmd::Attention(AttentionArgs {
            cmd: AttentionCmd::Show { id, diff },
        })) = shown.cmd
        else {
            panic!("attention show command")
        };
        assert_eq!(id, "att-1");
        assert!(diff);

        for verb in ["complete", "discard"] {
            assert!(Cli::try_parse_from(["cyclops", "attention", verb]).is_err());
            assert!(Cli::try_parse_from(["cyclops", "attention", verb, "m-1"]).is_ok());
        }
    }

    #[test]
    fn attention_diff_is_computed_locally() {
        assert_eq!(
            local_line_diff("same\nold\ntail", "same\nnew\ntail"),
            "--- expected\n+++ composer\n  same\n- old\n+ new\n  tail\n"
        );
    }

    /// Preview needs an explicit age, and requeue an explicit message.
    /// Neither operator command has a default that acts on everything.
    #[test]
    fn operator_commands_require_an_explicit_target() {
        assert!(Cli::try_parse_from(["cyclops", "alarm", "preview"]).is_err());
        assert!(
            Cli::try_parse_from(["cyclops", "alarm", "preview", "--older-than", "30m"]).is_ok()
        );
        assert!(Cli::try_parse_from(["cyclops", "requeue"]).is_err());
        assert!(Cli::try_parse_from(["cyclops", "requeue", "m-1"]).is_ok());
    }

    #[test]
    fn messages_command_keeps_the_settled_bound() {
        let default = Cli::try_parse_from(["cyclops", "messages"]).unwrap();
        let Some(Cmd::Messages(args)) = default.cmd else {
            panic!("messages command")
        };
        assert_eq!(args.recent_settled, 20);

        let given = Cli::try_parse_from(["cyclops", "messages", "--recent-settled", "7"]).unwrap();
        let Some(Cmd::Messages(args)) = given.cmd else {
            panic!("messages command")
        };
        assert_eq!(args.recent_settled, 7);
    }

    fn pending_row_to(
        recipient: cyclops_proto::RecipientKey,
        message_id: &str,
        fifo_position: u64,
        state: cyclops_proto::MessageNotificationState,
        cause: Option<cyclops_proto::NotificationAttentionCause>,
    ) -> cyclops_proto::MessageSnapshotRow {
        let workspace: cyclops_proto::WorkspaceId =
            "00000000-0000-0000-0000-000000000001".parse().unwrap();
        cyclops_proto::MessageSnapshotRow {
            message_id: cyclops_proto::MessageId::new(message_id).unwrap(),
            seq: fifo_position,
            ts: fifo_position,
            kind: cyclops_proto::Kind::Msg,
            direction: cyclops_proto::MessageDirection::Outbound,
            sender: cyclops_proto::RecipientKey::admin(workspace),
            sender_label: "admin".into(),
            recipients: vec![cyclops_proto::MessageRecipientSummary {
                recipient,
                label: "reviewer".into(),
                direction: cyclops_proto::MessageDirection::Outbound,
                needs_action: false,
                can_manage_attention: false,
                can_withdraw_notification: false,
                current_route: None,
                available: true,
                mailbox: cyclops_proto::MailboxEntryState::Pending,
                fifo_position: Some(fifo_position),
                notification: cyclops_proto::MessageNotificationSummary {
                    state,
                    wake_block: None,
                    quota_state: None,
                    settlement: None,
                    operator_withdrawn: None,
                    attempt_id: None,
                    cause,
                    verify_outcome: None,
                    pre_write_cause: None,
                    pre_write_pane_width: None,
                    pre_write_required_pane_width: None,
                    pre_write_block: None,
                    attention_cleared: None,
                    resolution: None,
                    resolution_intent: None,
                    resolution_action_accepted: None,
                    resolution_consumption_observed: None,
                    updated_at: None,
                },
            }],
            subject: Some("Work".into()),
            reply_to: None,
            thread_root: cyclops_proto::MessageId::new(message_id).unwrap(),
            thread_message_count: 1,
            active: true,
            needs_action: false,
        }
    }

    fn notification_attempt(number: u64) -> cyclops_proto::NotificationAttemptId {
        cyclops_proto::NotificationAttemptId::parse(&format!(
            "att-00000000-0000-4000-8000-{number:012x}"
        ))
        .unwrap()
    }

    /// A queue that is not moving is named by its head and cause on every
    /// follower row and once in a summary line.
    #[test]
    fn followers_name_the_head_that_holds_them() {
        let workspace: cyclops_proto::WorkspaceId =
            "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session: cyclops_proto::SessionInstanceId =
            "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let recipient =
            cyclops_proto::RecipientKey::agent(workspace, session, "%1".parse().unwrap());
        let mut rows = vec![
            pending_row_to(
                recipient,
                "m-head",
                1,
                cyclops_proto::MessageNotificationState::AttentionRequired,
                Some(cyclops_proto::NotificationAttentionCause::VerifyFailed),
            ),
            pending_row_to(
                recipient,
                "m-second",
                2,
                cyclops_proto::MessageNotificationState::NotStarted,
                None,
            ),
            pending_row_to(
                recipient,
                "m-third",
                3,
                cyclops_proto::MessageNotificationState::NotStarted,
                None,
            ),
        ];
        rows[0].recipients[0].notification.attempt_id = Some(notification_attempt(1));

        let heads = held_heads(&rows);
        let head = heads.get(&recipient).expect("held head indexed");
        assert_eq!(head.message_id.as_str(), "m-head");
        assert_eq!(head.cause, "verify_failed");
        assert_eq!(head.waiting, 2);

        assert_eq!(
            held_queue_lines(&heads),
            vec![
                "held queue · reviewer · head m-head · verify_failed · 2 waiting · next: recipient retrieves the durable payload with cyclops inbox claim m-head; or admin: cyclops attention show att-00000000-0000-4000-8000-000000000001 --diff, then complete or discard when its checks authorize the action".to_string()
            ]
        );

        let follower = message_snapshot_line("m-third".into(), &rows[2], &heads);
        assert!(
            follower.contains("pending · 2 ahead · behind m-head (verify_failed)"),
            "follower must name the head and its cause: {follower}"
        );
        let head_line = message_snapshot_line("m-head".into(), &rows[0], &heads);
        assert!(
            head_line.contains("pending · oldest") && !head_line.contains("behind"),
            "the head is not behind itself: {head_line}"
        );

        // A moving queue has no held head and rows read as before.
        let moving = vec![pending_row_to(
            recipient,
            "m-only",
            1,
            cyclops_proto::MessageNotificationState::Queued,
            None,
        )];
        assert!(held_heads(&moving).is_empty());
        assert!(held_queue_lines(&held_heads(&moving)).is_empty());
    }

    #[test]
    fn held_queue_actions_match_the_exact_cause() {
        let workspace: cyclops_proto::WorkspaceId =
            "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session: cyclops_proto::SessionInstanceId =
            "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let recipient =
            cyclops_proto::RecipientKey::agent(workspace, session, "%1".parse().unwrap());

        let mut quota_held = pending_row_to(
            recipient,
            "m-quota-held",
            1,
            cyclops_proto::MessageNotificationState::AttentionRequired,
            None,
        );
        quota_held.recipients[0].notification.quota_state =
            Some(cyclops_proto::MessageQuotaState::Held);
        let line = held_queue_lines(&held_heads(&[quota_held]))
            .into_iter()
            .next()
            .unwrap();
        assert!(line.contains("quota_held"), "{line}");
        assert!(line.contains("wait for quota reset"), "{line}");
        assert!(line.contains("cyclops requeue m-quota-held"), "{line}");
        assert!(!line.contains("attention show"), "{line}");

        let mut quota_reset = pending_row_to(
            recipient,
            "m-quota-reset",
            1,
            cyclops_proto::MessageNotificationState::AttentionRequired,
            None,
        );
        quota_reset.recipients[0].notification.quota_state =
            Some(cyclops_proto::MessageQuotaState::ResetObserved);
        let line = held_queue_lines(&held_heads(&[quota_reset]))
            .into_iter()
            .next()
            .unwrap();
        assert!(line.contains("quota_reset_observed"), "{line}");
        assert!(line.contains("cyclops requeue m-quota-reset"), "{line}");
        assert!(!line.contains("wait for quota reset"), "{line}");

        let mut blocked = pending_row_to(
            recipient,
            "m-blocked",
            1,
            cyclops_proto::MessageNotificationState::Gating,
            None,
        );
        blocked.recipients[0].notification.attempt_id = Some(notification_attempt(2));
        blocked.recipients[0].notification.pre_write_cause =
            Some(cyclops_proto::NotificationPreWriteCause::WorkerFailed);
        let line = held_queue_lines(&held_heads(&[blocked.clone()]))
            .into_iter()
            .next()
            .unwrap();
        assert!(line.contains("fix worker_failed"), "{line}");

        // The named block is preferred over the enum cause it was recorded under.
        blocked.recipients[0].notification.pre_write_cause =
            Some(cyclops_proto::NotificationPreWriteCause::WriteReadinessChanged);
        blocked.recipients[0].notification.pre_write_block = Some("hook_admission_unproven".into());
        let line = held_queue_lines(&held_heads(&[blocked]))
            .into_iter()
            .next()
            .unwrap();
        assert!(line.contains("hook_admission_unproven"), "{line}");
        assert!(!line.contains("write_readiness_changed"), "{line}");
        assert!(line.contains("cyclops inbox claim m-blocked"), "{line}");
        assert!(
            line.contains(
                "cyclops notification withdraw att-00000000-0000-4000-8000-000000000002 --recipient"
            ),
            "{line}"
        );
        assert!(!line.contains("requeue"), "{line}");

        let mut attention = pending_row_to(
            recipient,
            "m-attention",
            1,
            cyclops_proto::MessageNotificationState::AttentionRequired,
            Some(cyclops_proto::NotificationAttentionCause::VerifyFailed),
        );
        attention.recipients[0].notification.attempt_id = Some(notification_attempt(3));
        let line = held_queue_lines(&held_heads(&[attention.clone()]))
            .into_iter()
            .next()
            .unwrap();
        assert!(line.contains("cyclops inbox claim m-attention"), "{line}");
        assert!(
            line.contains("cyclops attention show att-00000000-0000-4000-8000-000000000003 --diff"),
            "{line}"
        );
        assert!(line.contains("complete or discard"), "{line}");
        assert!(!line.contains("requeue"), "{line}");

        let mut resolving = attention.clone();
        resolving.recipients[0].notification.resolution_intent =
            Some(cyclops_proto::NotificationResolution::Complete);
        let line = held_queue_lines(&held_heads(&[resolving]))
            .into_iter()
            .next()
            .unwrap();
        assert!(line.contains("attention_resolution_pending"), "{line}");
        assert!(line.contains("do not repeat a terminal action"), "{line}");
        assert!(!line.contains("then complete or discard"), "{line}");

        attention.recipients[0].notification.resolution =
            Some(cyclops_proto::NotificationResolution::Complete);
        assert!(
            held_heads(&[attention]).is_empty(),
            "a resolved compatibility attention state is not a held head"
        );
    }

    /// Clearing an alarm acknowledges it and retires nothing. The command
    /// names the body-free facts returned atomically by the daemon.
    #[test]
    fn alarm_clear_prints_consequence_and_next_action() {
        let alarm = cyclops_proto::AlarmSummary {
            id: "att-1".into(),
            message_id: "m-head".into(),
            recipient: "codey".into(),
            state: cyclops_proto::DeliveryState::AttentionRequired,
            cause: cyclops_proto::NotificationAttentionCause::VerifyFailed,
            ts: 7,
        };
        let line = alarm_cleared_consequence(&alarm);
        for needle in [
            "acknowledged only",
            "at clearance, attempt att-1 was attention_required (verify_failed)",
            "clearance did not change message m-head to codey",
            "while pending, it can hold that recipient's queue",
            "recipient retrieves the durable payload with cyclops inbox claim m-head",
            "cyclops attention show att-1 --diff",
            "neither clearance nor payload retrieval alone proves",
        ] {
            assert!(line.contains(needle), "missing {needle:?} in {line}");
        }
        assert!(
            !line.contains("requeue"),
            "a cleared attempt is not eligible for requeue: {line}"
        );

        let fallback = copy::alarm_cleared_without_summary("att-old");
        assert!(fallback.contains("acknowledged only"), "{fallback}");
        assert!(
            fallback.contains("attention show att-old --diff"),
            "{fallback}"
        );
    }

    #[test]
    fn messages_plain_line_and_json_name_the_same_body_free_state() {
        let workspace: cyclops_proto::WorkspaceId =
            "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session: cyclops_proto::SessionInstanceId =
            "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let sender = cyclops_proto::RecipientKey::admin(workspace);
        let recipient =
            cyclops_proto::RecipientKey::agent(workspace, session, "%1".parse().unwrap());
        let attempt =
            cyclops_proto::NotificationAttemptId::parse("att-00000000-0000-4000-8000-000000000001")
                .unwrap();
        let message_id = cyclops_proto::MessageId::new("m-1").unwrap();
        let row = cyclops_proto::MessageSnapshotRow {
            message_id: message_id.clone(),
            seq: 1,
            ts: 2,
            kind: cyclops_proto::Kind::Msg,
            direction: cyclops_proto::MessageDirection::Outbound,
            sender,
            sender_label: "admin".into(),
            recipients: vec![cyclops_proto::MessageRecipientSummary {
                recipient,
                label: "reviewer".into(),
                direction: cyclops_proto::MessageDirection::Outbound,
                needs_action: true,
                can_manage_attention: false,
                can_withdraw_notification: false,
                current_route: None,
                available: true,
                mailbox: cyclops_proto::MailboxEntryState::Pending,
                fifo_position: Some(2),
                notification: cyclops_proto::MessageNotificationSummary {
                    state: cyclops_proto::MessageNotificationState::AttentionRequired,
                    wake_block: None,
                    quota_state: None,
                    settlement: None,
                    operator_withdrawn: None,
                    attempt_id: Some(attempt),
                    cause: Some(cyclops_proto::NotificationAttentionCause::VerifyFailed),
                    verify_outcome: Some(cyclops_proto::NotificationVerifyOutcome {
                        kind: cyclops_proto::NotificationVerifyFailureKind::Mismatch,
                        observed_composer: cyclops_proto::ComposerState::HumanDraft,
                    }),
                    pre_write_cause: None,
                    pre_write_pane_width: None,
                    pre_write_required_pane_width: None,
                    pre_write_block: None,
                    attention_cleared: Some(false),
                    resolution: None,
                    resolution_intent: None,
                    resolution_action_accepted: None,
                    resolution_consumption_observed: None,
                    updated_at: Some(3),
                },
            }],
            subject: Some("Review".into()),
            reply_to: None,
            thread_root: message_id,
            thread_message_count: 3,
            active: true,
            needs_action: true,
        };

        let line = message_snapshot_line("m-1".into(), &row, &HeldHeads::new());
        assert_eq!(
            line,
            "m-1 outbound · work · admin -> reviewer [pending · 1 ahead; needs attention:verify_failed:verify=mismatch/human_draft:open att-00000000-0000-4000-8000-000000000001] · Review · thread 3"
        );
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["direction"], "outbound");
        assert_eq!(json["recipients"][0]["fifo_position"], 2);
        assert_eq!(
            json["recipients"][0]["notification"]["state"],
            "attention_required"
        );
        assert_eq!(
            json["recipients"][0]["notification"]["cause"],
            "verify_failed"
        );
        assert_eq!(
            json["recipients"][0]["notification"]["verify_outcome"]["kind"],
            "mismatch"
        );
        assert!(json.get("body").is_none());
        assert_eq!(
            attention_verify_failure_line(row.recipients[0].notification.verify_outcome).as_deref(),
            Some("verification failure: mismatch · composer human_draft")
        );

        let mut not_started = row.recipients[0].clone();
        not_started.fifo_position = Some(1);
        not_started.notification = cyclops_proto::MessageNotificationSummary {
            state: cyclops_proto::MessageNotificationState::NotStarted,
            wake_block: None,
            quota_state: None,
            settlement: None,
            operator_withdrawn: None,
            attempt_id: None,
            cause: None,
            verify_outcome: None,
            pre_write_cause: None,
            pre_write_pane_width: None,
            pre_write_required_pane_width: None,
            pre_write_block: None,
            attention_cleared: None,
            resolution: None,
            resolution_intent: None,
            resolution_action_accepted: None,
            resolution_consumption_observed: None,
            updated_at: None,
        };
        assert_eq!(
            message_recipient_cell(&row.message_id, &not_started, None),
            "reviewer [pending · oldest; not started]"
        );

        let mut gating = row.recipients[0].clone();
        gating.available = false;
        gating.notification = cyclops_proto::MessageNotificationSummary {
            state: cyclops_proto::MessageNotificationState::Gating,
            wake_block: None,
            quota_state: None,
            settlement: None,
            operator_withdrawn: None,
            attempt_id: Some(attempt),
            cause: None,
            verify_outcome: None,
            pre_write_cause: None,
            pre_write_pane_width: None,
            pre_write_required_pane_width: None,
            pre_write_block: None,
            attention_cleared: None,
            resolution: None,
            resolution_intent: None,
            resolution_action_accepted: None,
            resolution_consumption_observed: None,
            updated_at: Some(3),
        };
        assert_eq!(
            message_recipient_cell(&row.message_id, &gating, None),
            "reviewer [pending · 1 ahead; checking readiness att-00000000-0000-4000-8000-000000000001; unavailable]"
        );

        let mut held = gating.clone();
        held.available = true;
        held.notification.state = cyclops_proto::MessageNotificationState::AttentionRequired;
        held.notification.quota_state = Some(cyclops_proto::MessageQuotaState::Held);
        let held_cell = message_recipient_cell(&row.message_id, &held, None);
        assert!(held_cell.contains("wait for quota reset"), "{held_cell}");
        assert!(held_cell.contains("no automatic resume"), "{held_cell}");

        let mut reset = held.clone();
        reset.notification.quota_state = Some(cyclops_proto::MessageQuotaState::ResetObserved);
        let reset_cell = message_recipient_cell(&row.message_id, &reset, None);
        assert!(reset_cell.contains("cyclops requeue m-1"), "{reset_cell}");
        assert!(reset_cell.contains("message wide"), "{reset_cell}");

        let mut withdrawn = gating.clone();
        withdrawn.notification.state = cyclops_proto::MessageNotificationState::NotStarted;
        withdrawn.notification.settlement =
            Some(cyclops_proto::MessageNotificationSettlement::WithdrawnByClaim);
        let withdrawn_cell = message_recipient_cell(&row.message_id, &withdrawn, None);
        assert!(withdrawn_cell.contains("withdrawn"), "{withdrawn_cell}");
        assert!(!withdrawn_cell.contains("notified"), "{withdrawn_cell}");

        let mut resolved = row.recipients[0].clone();
        resolved.notification.resolution = Some(cyclops_proto::NotificationResolution::Complete);
        assert_eq!(
            message_recipient_cell(&row.message_id, &resolved, None),
            "reviewer [pending · 1 ahead; wake submitted att-00000000-0000-4000-8000-000000000001]"
        );
        assert_eq!(
            serde_json::to_value(gating.notification.state).unwrap(),
            "gating"
        );

        let mut uncertain = row.recipients[0].clone();
        uncertain.notification.resolution_intent =
            Some(cyclops_proto::NotificationResolution::Complete);
        let cell = message_recipient_cell(&row.message_id, &uncertain, None);
        assert!(cell.contains("terminal acceptance unproven"), "{cell}");
        assert!(!cell.contains("cyclops attention complete"), "{cell}");

        uncertain.notification.resolution_action_accepted =
            Some(cyclops_proto::NotificationResolution::Complete);
        let cell = message_recipient_cell(&row.message_id, &uncertain, None);
        assert!(
            cell.contains("terminal accepted, task start unproven"),
            "{cell}"
        );
        assert!(!cell.contains("cyclops attention complete"), "{cell}");

        uncertain.notification.resolution_consumption_observed = Some(
            cyclops_proto::NotificationResolutionConsumptionObservation {
                evidence: cyclops_proto::NotificationResolutionConsumptionEvidence::WorkingEdge,
                observed_at_ms: 4,
            },
        );
        let cell = message_recipient_cell(&row.message_id, &uncertain, None);
        assert!(cell.contains("terminal accepted the action key"), "{cell}");
        assert!(cell.contains("cyclops attention complete"), "{cell}");
        assert!(
            cell.contains("rechecks without sending a second key"),
            "{cell}"
        );

        uncertain.notification.resolution_intent =
            Some(cyclops_proto::NotificationResolution::Discard);
        uncertain.notification.resolution_action_accepted =
            Some(cyclops_proto::NotificationResolution::Discard);
        uncertain.notification.resolution_consumption_observed = None;
        let cell = message_recipient_cell(&row.message_id, &uncertain, None);
        assert!(cell.contains("terminal accepted the action key"), "{cell}");
        assert!(cell.contains("cyclops attention discard"), "{cell}");

        let mut worker_failed = gating.clone();
        worker_failed.can_withdraw_notification = true;
        worker_failed.current_route = Some(cyclops_proto::MessageRecipientRoute {
            label: "reviewer-now".into(),
            pane_id: "%1".parse().unwrap(),
        });
        worker_failed.notification.pre_write_cause =
            Some(cyclops_proto::NotificationPreWriteCause::WorkerFailed);
        let cell = message_recipient_cell(&row.message_id, &worker_failed, None);
        assert!(cell.contains("worker_failed"), "{cell}");
        assert!(cell.contains("waited "), "{cell}");
        assert!(cell.contains("current route reviewer-now (%1)"), "{cell}");
        assert!(
            cell.contains(
                "admin next: cyclops notification withdraw att-00000000-0000-4000-8000-000000000001"
            ),
            "{cell}"
        );

        worker_failed.notification.wake_block =
            Some(cyclops_proto::MessageWakeBlock::WorkerSupervisorExited);
        let cell = message_recipient_cell(&row.message_id, &worker_failed, None);
        assert!(cell.contains("worker_supervisor_exited"), "{cell}");
        assert!(!cell.contains("scheduler_state_unavailable"), "{cell}");
    }

    /// The preview line shows the cause in the protocol's own spelling.
    ///
    /// The word printed and the word in --json output have to be the
    /// same one, or an operator reading a script and an operator reading
    /// the terminal are looking at different vocabularies. The wire shape
    /// itself is asserted in cyclops-proto.
    #[test]
    fn a_previewed_alarm_prints_the_cause_the_protocol_named() {
        use cyclops_proto::NotificationAttentionCause;

        for (cause, expected) in [
            (NotificationAttentionCause::VerifyFailed, "verify_failed"),
            (NotificationAttentionCause::SubmitFailed, "submit_failed"),
            (NotificationAttentionCause::AckTimeout, "ack_timeout"),
        ] {
            assert_eq!(
                wire_word(serde_json::to_value(cause).unwrap()),
                expected,
                "{cause:?} printed under another name"
            );
        }

        // A value the daemon sent that is not a string is named, not
        // dropped: a blank column would read as an alarm with no cause.
        assert_eq!(wire_word(Value::Null), "unknown");

        // The whole line, so a cause dropped from the render is caught
        // rather than only a cause spelled wrong.
        let alarm = cyclops_proto::AlarmSummary {
            id: "att-00000000-0000-4000-8000-000000000001".into(),
            message_id: "m-1".into(),
            recipient: "reviewer".into(),
            state: DeliveryState::AttentionRequired,
            cause: NotificationAttentionCause::VerifyFailed,
            ts: 7,
        };
        assert_eq!(
            alarm_line("att-1", &alarm),
            "att-1 reviewer · m-1 · attention_required · verify_failed"
        );
    }

    fn receipt(state: DeliveryState) -> DeliveryReceipt {
        DeliveryReceipt {
            to: "reviewer".into(),
            state,
            notification_state: None,
            quota_state: None,
            notification_settlement: None,
            pre_write_cause: None,
            wake_block: None,
            position: None,
            note: None,
            pane: None,
            held_by: None,
        }
    }

    #[test]
    fn require_wake_accepts_only_mailbox_submit_success_states() {
        use MessageNotificationState::*;

        for (state, expected_exit) in [
            (NotStarted, 1),
            (Queued, 1),
            (Gating, 1),
            (Writing, 1),
            (Staged, 1),
            (Submitted, 0),
            (Notified, 0),
            (AttentionRequired, 1),
            (Superseded, 1),
        ] {
            let mut mailbox = receipt(DeliveryState::Queued);
            mailbox.notification_state = Some(state);
            assert_eq!(receipts_exit(&[mailbox], true), expected_exit, "{state:?}");
        }
    }

    #[test]
    fn require_wake_json_matches_typed_receipts_and_fails_closed() {
        use MessageNotificationState::*;

        for state in [
            NotStarted,
            Queued,
            Gating,
            Writing,
            Staged,
            Submitted,
            Notified,
            AttentionRequired,
            Superseded,
        ] {
            let mut mailbox = receipt(DeliveryState::Queued);
            mailbox.notification_state = Some(state);
            let expected = receipts_exit(&[mailbox], true);
            assert_eq!(
                receipts_exit_json(
                    &json!({
                        "msg_id": "m-1",
                        "seq": 1,
                        "deliveries": [{
                            "to": "reviewer",
                            "state": "queued",
                            "notification_state": serde_json::to_value(state).unwrap()
                        }]
                    }),
                    true,
                ),
                expected,
                "{state:?}"
            );
        }

        for result in [
            json!({
                "msg_id": "m-1",
                "seq": 1,
                "deliveries": [{
                    "to": "reviewer",
                    "state": "queued",
                    "notification_state": "from_next_year"
                }]
            }),
            json!({
                "msg_id": "m-1",
                "seq": 1,
                "deliveries": [{"to": "reviewer", "state": "from_next_year"}]
            }),
            json!({"msg_id": "m-1", "seq": 1}),
            json!({"msg_id": "m-1", "seq": 1, "deliveries": []}),
        ] {
            assert_eq!(receipts_exit_json(&result, true), 1, "{result}");
        }
        assert_eq!(receipts_exit(&[], true), 1);
    }

    #[test]
    fn require_wake_accepts_only_proven_legacy_direct_states() {
        use DeliveryState::*;
        for (state, expected_exit) in [
            (Queued, 1),
            (Gating, 1),
            (Pasting, 1),
            (Staged, 1),
            (Submitted, 0),
            (DeliveredVerified, 0),
            (DeliveredUnverified, 0),
            (RetryQueued, 1),
            (AttentionRequired, 1),
            (ParkedBlockedQuota, 1),
        ] {
            assert_eq!(
                receipts_exit(&[receipt(state)], true),
                expected_exit,
                "{state:?}"
            );
        }
    }

    #[test]
    fn require_wake_rejects_blocks_and_unproven_broadcast_recipients() {
        let mut submitted = receipt(DeliveryState::Queued);
        submitted.notification_state = Some(MessageNotificationState::Submitted);
        let notified = DeliveryReceipt {
            notification_state: Some(MessageNotificationState::Notified),
            ..submitted.clone()
        };
        assert_eq!(receipts_exit(&[submitted.clone(), notified], true), 0);

        let queued = receipt(DeliveryState::Queued);
        assert_eq!(receipts_exit(&[submitted.clone(), queued.clone()], true), 1);

        let mut wake_blocked = submitted.clone();
        wake_blocked.wake_block = Some(cyclops_proto::MessageWakeBlock::EnqueueRefused);
        assert_eq!(receipts_exit(&[wake_blocked], true), 1);

        let mut pre_write_blocked = submitted.clone();
        pre_write_blocked.pre_write_cause =
            Some(cyclops_proto::NotificationPreWriteCause::BindingUnprovable);
        assert_eq!(receipts_exit(&[pre_write_blocked], true), 1);

        let mut human_required = submitted;
        human_required.state = DeliveryState::AttentionRequired;
        assert_eq!(receipts_exit(&[human_required], true), 1);
    }

    #[test]
    fn accepted_send_defaults_to_success_for_any_current_wake_receipt() {
        let mut blocked = receipt(DeliveryState::Queued);
        blocked.wake_block = Some(cyclops_proto::MessageWakeBlock::EnqueueRefused);

        assert_eq!(receipts_exit(&[blocked.clone()], false), 0);
        assert_eq!(receipts_exit(&[blocked], true), 1);
        for raw in [
            json!({
                "msg_id": "m-blocked",
                "seq": 1,
                "deliveries": [{
                    "to": "reviewer",
                    "state": "queued",
                    "wake_block": "enqueue_refused"
                }]
            }),
            json!({
                "msg_id": "m-future",
                "seq": 2,
                "deliveries": [{"to": "reviewer", "state": "from_next_year"}]
            }),
        ] {
            assert_eq!(receipts_exit_json(&raw, false), 0, "{raw}");
        }
        assert_eq!(receipts_exit_json(&json!({}), false), 1);
    }

    #[test]
    fn default_acceptance_envelope_validates_only_durable_acceptance_fields() {
        let future_receipt = json!({
            "msg_id": "m-future",
            "seq": 1,
            "inserted": false,
            "deliveries": [{"to": "reviewer", "state": "from_next_year"}]
        });
        let acceptance = message_acceptance(&future_receipt).expect("valid acceptance");
        assert_eq!(acceptance.msg_id.as_str(), "m-future");
        assert_eq!(acceptance.inserted, Some(false));

        for invalid in [
            json!({}),
            json!({"msg_id": "future", "seq": 1, "deliveries": []}),
            json!({"msg_id": "m-future", "seq": 0, "deliveries": []}),
            json!({"msg_id": "m-future", "seq": "1", "deliveries": []}),
            json!({"msg_id": "m-future", "seq": 1}),
            json!({"msg_id": "m-future", "seq": 1, "deliveries": {}}),
        ] {
            assert!(message_acceptance(&invalid).is_none(), "{invalid}");
            assert_eq!(receipts_exit_json(&invalid, false), 1, "{invalid}");
        }
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
