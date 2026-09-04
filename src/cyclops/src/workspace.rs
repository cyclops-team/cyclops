//! Workspaces: the layout a session is built from, where it is kept, and
//! the three verbs over it.
//!
//! A workspace is one declarative file: windows, rows, panes, ratios,
//! labels, working directories, and a launch hint per pane. It lives at
//! `$CYCLOPS_HOME/workspaces/<name>.toml`. The four presets ship in
//! `layouts/` and are compiled into this binary, so a fresh install has
//! them before it has a config file.
//!
//! The file is the structure, never the processes. `restore` rebuilds
//! panes, sizes, directories and labels; it starts nothing unless you pass
//! `--launch`, and even then tmux runs the command as the pane's own, with
//! no keys sent anywhere.
//!
//! Labels are the reason this is more than a tmux script. The daemon's
//! registry keeps a name attached to a pane that is still alive, across a
//! daemon restart; it prunes an entry whose pane is gone, because a tmux
//! pane id is reused by the next server. A workspace file is the other
//! half: it keeps the names when the panes themselves are gone, so
//! `restore` brings back the roster and not just the rectangles.
//!
//! Division of labor: everything tmux-shaped is `cyclops_tmux::layout`
//! (the adapter is the only place tmux specifics may live). This module
//! owns files, the config keys, the daemon round trips, and the copy.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cyclops_state::{CreateFileOutcome, StateRoot};
use cyclops_tmux::layout::{self, ApplyOptions, Capture, Layout, Server};
use serde_json::{json, Value};

use crate::client::{Client, ClientError};
use crate::style::Style;

/// The shipped presets, in ladder order: each is the one before it plus a
/// pane. Compiled in rather than read from disk so `cyclops start` works
/// on an install that is nothing but two binaries.
const PRESETS: &[(&str, &str)] = &[
    ("solo", include_str!("../../../resources/layouts/solo.toml")),
    ("duo", include_str!("../../../resources/layouts/duo.toml")),
    ("quad", include_str!("../../../resources/layouts/quad.toml")),
    ("ops", include_str!("../../../resources/layouts/ops.toml")),
];

/// Fallback workspace name when neither the config nor the command line
/// names one. Matches the session name every doc page uses.
const DEFAULT_WORKSPACE: &str = "main";

/// Preset `cyclops start` builds when there is nothing saved. The ladder
/// starts at one pane so the first-run workspace is useful for one agent.
const FIRST_RUN_PRESET: &str = "solo";

pub fn preset_names() -> Vec<&'static str> {
    PRESETS.iter().map(|(n, _)| *n).collect()
}

/// The shipped preset that names exactly `n` agents, if there is one. For
/// the refusal that has to say what would fit.
fn preset_with_agents(n: usize) -> Option<&'static str> {
    PRESETS
        .iter()
        .find(|(_, text)| parse(text).map(|l| l.agents()) == Ok(n))
        .map(|(name, _)| *name)
}

/// One shipped preset by name.
pub fn preset(name: &str) -> Option<Result<Layout, String>> {
    let (_, text) = PRESETS.iter().find(|(n, _)| *n == name)?;
    Some(parse(text).map_err(|e| format!("built-in preset \"{name}\" is broken: {e}")))
}

/// Parse a layout file and reject one that cannot be built.
///
/// Both halves matter: TOML that does not parse, and TOML that parses into
/// a layout with an empty window or a label used twice.
pub fn parse(text: &str) -> Result<Layout, String> {
    let layout: Layout = toml::from_str(text).map_err(|e| e.message().to_string())?;
    layout.validate()?;
    Ok(layout)
}

/// Where a saved workspace lives.
pub fn path_of(home: &Path, name: &str) -> PathBuf {
    home.join("workspaces").join(format!("{name}.toml"))
}

/// Load a saved workspace. None when there is no file by that name.
pub fn load(home: &Path, name: &str) -> Option<Result<Layout, String>> {
    let path = path_of(home, name);
    let text = std::fs::read_to_string(&path).ok()?;
    Some(parse(&text).map_err(|e| format!("{}: {e}", path.display())))
}

/// Write a workspace file, creating the directory.
fn store(home: &Path, name: &str, layout: &Layout) -> Result<PathBuf, String> {
    let path = path_of(home, name);
    let body = toml::to_string_pretty(layout).map_err(|e| e.to_string())?;
    let text = format!("{}{body}", saved_header(name));
    StateRoot::open_or_create(home)
        .and_then(|root| {
            root.replace_file(
                &Path::new("workspaces").join(format!("{name}.toml")),
                text.as_bytes(),
            )
        })
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn saved_header(name: &str) -> String {
    format!(
        "# Cyclops workspace: {name}\n\
         # Saved by `cyclops workspace save`. Structure, labels, directories\n\
         # and launch hints; edit it like any other config file.\n\
         # Restore it with: cyclops workspace restore {name}\n\n"
    )
}

/// A name that is safe as both a file name and a tmux session name.
///
/// Two concrete failures: a name holding `/` or `..` writes a workspace
/// file outside the workspaces directory, and tmux reads `:` and `.` as
/// window and pane separators inside a target, so a session named with
/// either can never be addressed again.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a workspace name can't be empty.".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "\"{name}\" can't be a workspace name. Use letters, digits, dashes and underscores."
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// The config values these verbs need.
///
/// This launcher owns `default_workspace`: it parses that key when a
/// workspace command begins, supplies the fallback order below, and writes
/// the first-run value. The shared file also carries coordinator settings
/// needed to construct a workspace on the daemon's tmux server. Those three
/// settings are accepted only by the shared safety boundary, before a CLI
/// command can select tmux's ambient server.
pub struct Settings {
    /// `sessions`, in order.
    pub sessions: Vec<String>,
    /// `default_workspace`.
    pub default_workspace: Option<String>,
    /// `tmux_socket` and `tmux_config`, so a workspace is built on the
    /// same server the daemon watches.
    pub server: Server,
    /// Whether config.toml exists. First run is its absence.
    pub exists: bool,
}

impl Settings {
    pub fn read(home: &Path) -> Result<Settings, String> {
        let document = cyclops_state::load_coordinator_config(home)
            .map_err(|error| format!("read {}: {error}", config_path(home).display()))?;
        let mut settings = Settings {
            sessions: document.coordinator.sessions,
            default_workspace: None,
            server: Server {
                socket: document.coordinator.tmux_socket,
                config_file: document.coordinator.tmux_config,
            },
            exists: document.exists,
        };
        settings.default_workspace = document
            .table
            .get("default_workspace")
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        Ok(settings)
    }

    /// Which workspace a bare `cyclops start` means: the explicit ask, the
    /// configured default, the first watched session, then "main".
    pub fn workspace_name(&self, asked: Option<&str>) -> String {
        asked
            .or(self.default_workspace.as_deref())
            .or(self.sessions.first().map(String::as_str))
            .unwrap_or(DEFAULT_WORKSPACE)
            .to_string()
    }

    pub fn watches(&self, session: &str) -> bool {
        self.sessions.iter().any(|s| s == session)
    }
}

/// Bind a semantic pane-focus request to this launcher's configured server.
#[cfg(feature = "full-ui")]
pub fn focus_adapter(home: &Path) -> Result<cyclops_ui::FocusPane, String> {
    let server = Settings::read(home)?.server;
    Ok(cyclops_ui::FocusPane::new(move |target| {
        cyclops_tmux::focus_pane(
            server.socket.as_deref(),
            server.config_file.as_deref(),
            target,
        )
        .map_err(|error| error.to_string())
    }))
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join(cyclops_state::COORDINATOR_CONFIG_FILE)
}

/// The size to build a detached session at: this terminal's, when there
/// is one.
///
/// Measured on tmux 3.6a: tmux hands the cells from a window resize
/// out EVENLY, not in proportion. A two-pane window split 70/29 at 100
/// columns becomes 120/79 at 200, not 140/59. So a workspace built at
/// tmux's 80x24 default and attached from a 200x50 terminal loses the
/// ratios it was designed with: the `ops` dock arrives at 41% of the
/// screen instead of 30%. Building at the size of the terminal the
/// operator typed into is what keeps a preset looking like its design.
///
/// None when stdout is not a terminal, which leaves the size to tmux.
fn build_size() -> Option<(u32, u32)> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let (w, h) = cyclops_ui::terminal_size();
    Some((w as u32, h as u32))
}

/// The config a first run writes. Two keys: what to watch, and what
/// `cyclops start` means.
pub fn first_run_config(session: &str) -> String {
    format!(
        "# Cyclops config. Data only; nothing in it executes.\n\
         # Written by `cyclops start`. Every key: docs/guides/install.md\n\
         \n\
         sessions = [\"{session}\"]\n\
         default_workspace = \"{session}\"\n"
    )
}

/// The line to add by hand when a config already exists but does not name
/// the session. `start` never edits a config you wrote: an array you
/// maintain is yours, and rewriting the file would cost you its comments.
pub fn config_hint(path: &Path, session: &str) -> String {
    format!(
        "cyclopsd won't watch \"{session}\" until it's listed in {}. Add it to sessions there, then restart cyclopsd.",
        path.display()
    )
}

// ---------------------------------------------------------------------------
// The daemon's half
// ---------------------------------------------------------------------------

/// What the daemon says about one session.
enum Watch {
    /// Not running.
    Down,
    /// Running, but this session is not one it was told to watch.
    Elsewhere,
    /// Told to watch it, not connected to it yet. A session created a
    /// moment ago remains here until `cyclops start` sends its idempotent
    /// `session.watch` creation edge and the daemon attaches. Until then it
    /// can resolve none of the session's panes, so naming one would fail per
    /// pane with nothing useful to say.
    NotYet,
    /// Watching it. Carries pane id to name for the session.
    Watching(BTreeMap<String, String>),
}

impl Watch {
    /// One word for `--json`, so a script can branch on the same four
    /// states the copy distinguishes.
    fn word(&self) -> &'static str {
        match self {
            Watch::Down => "down",
            Watch::Elsewhere => "elsewhere",
            Watch::NotYet => "connecting",
            Watch::Watching(_) => "watching",
        }
    }
}

/// Ask the daemon about a session. Never fatal: every verb here works
/// without a daemon, minus the names.
fn watch_of(client: &mut Client, session: &str) -> Watch {
    session_view(client, session).0
}

/// Tell a live daemon that a caller has just created or restored this tmux
/// session. The request is idempotent for configured slots and opens a
/// runtime slot only when the daemon did not know this name yet.
fn notify_session_available(client: &mut Client, session: &str) -> bool {
    client
        .request("session.watch", json!({"session": session}))
        .is_ok()
}

/// One status request, answering both questions callers ask of it: what
/// the daemon is doing with this session, and which panes it can see.
///
/// The pane set is separate from the names because they are different
/// facts. A pane the daemon has not read yet carries no name and cannot be
/// given one, so a caller about to label panes has to check the set and
/// not the names.
fn session_view(client: &mut Client, session: &str) -> (Watch, HashSet<String>) {
    let none = HashSet::new();
    let Ok(v) = client.request("status", json!({})) else {
        return (Watch::Down, none);
    };
    let Some(sessions) = v.get("sessions").and_then(Value::as_array) else {
        return (Watch::Down, none);
    };
    let Some(s) = sessions.iter().find(|s| s["name"] == json!(session)) else {
        return (Watch::Elsewhere, none);
    };
    if s["attached"] != json!(true) {
        return (Watch::NotYet, none);
    }
    let mut labels = BTreeMap::new();
    let mut panes = HashSet::new();
    for p in s["panes"].as_array().into_iter().flatten() {
        let Some(id) = p["pane_id"].as_str() else {
            continue;
        };
        panes.insert(id.to_string());
        if let Some(agent) = p["agent"].as_str() {
            labels.insert(id.to_string(), agent.to_string());
        }
    }
    (Watch::Watching(labels), panes)
}

/// How long `start` waits for the daemon to consume its explicit creation
/// notification and read the pane table. This is bounded client patience, not
/// a daemon retry cadence for a missing session.
const ATTACH_WAIT: Duration = Duration::from_secs(6);

/// Wait for cyclopsd to reach a session and read its panes, or give up.
///
/// Attaching is not enough to wait for. The daemon reads the pane table
/// after it attaches, and a pane it has not read yet cannot be labelled:
/// naming one answers `no such target "%1"`. So the wait ends when every
/// pane this run built is one the daemon can see.
///
/// Giving up is not a failure. The caller reports what it could confirm
/// and prints the step that names the panes once the daemon catches up,
/// which is better than a terminal that never comes back.
fn wait_for_attach(client: &mut Client, session: &str, built: &[String]) -> Watch {
    let deadline = Instant::now() + ATTACH_WAIT;
    loop {
        let (watch, panes) = session_view(client, session);
        let ready =
            matches!(watch, Watch::Watching(_)) && built.iter().all(|id| panes.contains(id));
        if ready || !matches!(watch, Watch::NotYet | Watch::Watching(_)) {
            return watch;
        }
        if Instant::now() >= deadline {
            return watch;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Connect without printing. A down daemon is a normal state for these
/// verbs, not an error.
fn daemon() -> Option<Client> {
    match Client::connect() {
        Ok(c) => Some(c),
        Err(ClientError::NotRunning(_)) => None,
        // Anything else (a wedged accept queue, a broken hello) is also a
        // daemon we cannot ask. The verb says so in its own copy.
        Err(_) => None,
    }
}

/// Put the workspace's labels back on its panes.
///
/// `pane_ids` is in [`Layout::panes`] order, which is position order:
/// rows top to bottom, panes left to right. The caller has already run
/// [`Naming`]'s gates, because mapping labels onto a session that moved
/// would name the wrong pane, and a message to the wrong pane is the one
/// failure Cyclops must never have. Returns the pane ids that now carry a
/// name, so the caller can count them alongside the ones the daemon
/// already knew.
fn adopt(client: &mut Client, layout: &Layout, pane_ids: &[String]) -> (Vec<String>, Vec<String>) {
    let mut named = Vec::new();
    let mut problems = Vec::new();
    for (pane, id) in layout.panes().zip(pane_ids) {
        let Some(label) = &pane.label else { continue };
        match client.request("pane.label", json!({"target": id, "label": label})) {
            Ok(_) => named.push(id.clone()),
            Err(ClientError::Server { message, .. }) => problems.push(message),
            Err(e) => problems.push(crate::copy::client_error(&e, None)),
        }
    }
    (named, problems)
}

// ---------------------------------------------------------------------------
// May this workspace name this session's panes?
// ---------------------------------------------------------------------------

/// The answer, and the sentence to print when it is no.
///
/// A workspace file holds no pane ids, and cannot: the panes it describes
/// are usually gone by the time it is read, and tmux hands a freed id to
/// the next pane it makes. All it has to point at a pane with is WHERE
/// the pane sits. That points at something only while the two grids agree
/// at every level AND every name already on a pane is where the file puts
/// it, so both are checked before anything is renamed.
///
/// Pane COUNT is not that check. An `ops` workspace (three across, one
/// dock) and a `quad` (two by two) both hold four panes, and mapping
/// either onto the other renames every agent onto a pane it does not own.
/// A name is what every later delivery resolves through, so the next
/// message would go to the wrong agent. Cyclops must never type into the
/// wrong pane.
enum Naming {
    /// The panes are where the workspace says they are.
    Allowed,
    /// Refused. Nothing is renamed, and this says why and what fixes it.
    Refused(String),
}

/// The first place the live grid stops matching the workspace's, or None
/// when they agree pane for pane.
///
/// Ratios are deliberately not compared. Resizing a pane moves no agent,
/// and tmux hands out the cells from a window resize evenly rather than
/// in proportion, so a workspace built at one terminal size never
/// has the other's cell counts anyway.
fn first_difference(live: &Layout, want: &Layout) -> Option<String> {
    if live.windows.len() != want.windows.len() {
        return Some(format!(
            "it has {} and the workspace describes {}",
            count(live.windows.len(), "window"),
            count(want.windows.len(), "window")
        ));
    }
    for (l, w) in live.windows.iter().zip(&want.windows) {
        if l.rows.len() != w.rows.len() {
            return Some(format!(
                "window \"{}\" has {} and the workspace describes {}",
                l.name,
                count(l.rows.len(), "row"),
                count(w.rows.len(), "row")
            ));
        }
        for (i, (lr, wr)) in l.rows.iter().zip(&w.rows).enumerate() {
            if lr.panes.len() != wr.panes.len() {
                return Some(format!(
                    "row {} of window \"{}\" has {} and the workspace describes {}",
                    i + 1,
                    l.name,
                    count(lr.panes.len(), "pane"),
                    count(wr.panes.len(), "pane")
                ));
            }
        }
    }
    None
}

/// Names cyclopsd already holds in this session that are not on the pane
/// the workspace puts them on.
///
/// This is the check the grid cannot make. Two panes swapped with tmux's
/// `swap-pane` leave the grid identical and take their names with them, so
/// naming by position would hand each agent the other's name. A name that
/// is already sitting somewhere else means this is not the session the
/// workspace describes, whatever its shape says.
fn misplaced_names(
    layout: &Layout,
    pane_ids: &[String],
    live: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (pane, id) in layout.panes().zip(pane_ids) {
        let Some(held) = live.get(id) else { continue };
        match &pane.label {
            Some(want) if want == held => {}
            Some(want) => out.push(format!(
                "{id} answers to \"{held}\" and the workspace puts \"{want}\" there"
            )),
            None => out.push(format!(
                "{id} answers to \"{held}\" and the workspace names no agent there"
            )),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

/// What a `cyclops start` runs in the panes it builds.
///
/// The two halves are deliberately different decisions, and they are two
/// flags because they are answered at different scopes. `stored` replays
/// commands a file already holds, all of them, and that stays an explicit
/// per-run choice (`cyclops_tmux::layout`, `push_pane_args`): a file naming
/// a command is a suggestion. `agents` is the operator typing CLI names at
/// the keyboard right now, which IS that decision for the panes it names,
/// so those panes carry it themselves (`layout::Pane::launch_now`) and need
/// no second flag.
///
/// Neither weakens the other, and both hold in one run: `--agents` starts
/// what it just wrote and leaves every other command in the file waiting
/// for `--launch`, including the commands `--agents` itself wrote on an
/// earlier run.
pub struct Launch<'a> {
    /// `--launch`.
    pub stored: bool,
    /// `--agents`: manifest ids, one per pane the workspace names, in
    /// layout order.
    pub agents: &'a [String],
}

impl Launch<'_> {
    /// Whether this build replays the commands the layout came in with.
    /// Only `--launch` does. `--agents` is not a quieter way to ask for it:
    /// the panes it filled are marked one by one, so an unnamed pane's
    /// stored command is untouched by it.
    fn replays_stored(&self) -> bool {
        self.stored
    }
}

/// Resolve `--agents claude,codex` into one launch command per named pane.
///
/// Every refusal is here, before anything is built, because a session half
/// opened around a typo is worse than no session: it is one the operator
/// has to take down before trying again.
///
/// Three refusals, and each is a different mistake. An id no manifest
/// knows is usually a typo. An id whose manifest carries no `launch` is a
/// CLI cyclops can watch but was never told how to start. A count that
/// does not fit the workspace's named panes has no correct answer at all:
/// filling some panes and not others would put a fleet up that is quietly
/// smaller than the one that was asked for.
fn resolve_agents(
    home: &Path,
    ids: &[String],
    layout: &Layout,
    source: &str,
) -> Result<Vec<String>, String> {
    let known = crate::manifests::known(home);
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        // A list typed with spaces after its commas is the same list.
        let id = id.trim();
        if id.is_empty() {
            return Err(empty_agent_id());
        }
        match known.get(id) {
            None => return Err(unknown_agent(id, &known)),
            Some(m) => match &m.launch {
                None => return Err(agent_wont_launch(id, &m.path)),
                Some(cmd) => out.push((id.to_string(), cmd.clone())),
            },
        }
    }
    let labels: Vec<String> = layout.panes().filter_map(|p| p.label.clone()).collect();
    if out.len() != labels.len() {
        return Err(agent_count_mismatch(out.len(), source, &labels));
    }
    // Hand each pane its own hook config, for the CLIs that take one as a
    // launch flag. This runs after every refusal above, so a workspace that
    // is not going to be built writes no artifacts on the way to saying so.
    Ok(out
        .into_iter()
        .zip(&labels)
        .map(|((id, cmd), label)| wire_hooks(home, &known, &id, cmd, label))
        .collect())
}

/// Append the pane's own hook config to its launch command, for a CLI whose
/// manifest names a flag that takes one.
///
/// Cyclops-created Claude panes use an isolated settings file. Normal direct
/// Claude launches use the safely merged default settings file instead. A CLI
/// that discovers hooks from a fixed path of its own declares no flag and
/// passes through here unchanged.
///
/// Failure returns the bare command rather than an error. A pane that starts
/// unwired is exactly what every pane did before this existed, and it stays
/// visible: the agent runs, its deliveries fall to the screen-verified tier,
/// and `cyclops hooks selftest <label>` says so. Refusing to open the
/// workspace over an unwritable hook file would trade a working fleet for a
/// missing one.
fn wire_hooks(
    home: &Path,
    known: &BTreeMap<String, crate::manifests::Known>,
    id: &str,
    cmd: String,
    label: &str,
) -> String {
    // The pane's own hook config, for a CLI that takes one as a launch flag
    // rather than discovering it from a fixed path of its own.
    //
    // The pane's identity is not here. The daemon derives it from the hook
    // process and its admitted agent ancestor. CYCLOPS_AGENT only namespaces
    // the optional hook sequence counter.
    let Some(flag) = known.get(id).and_then(|m| m.settings_flag.as_deref()) else {
        return cmd;
    };
    let Some(kind) = crate::hookset::CliKind::from_name(id) else {
        return cmd;
    };
    match crate::hookset::prepare(home, kind, label) {
        Ok(path) => format!("{cmd} {flag} {}", sh_quote(&path.display().to_string())),
        Err(_) => cmd,
    }
}

/// Single-quote a path for the shell tmux runs the pane command with.
///
/// The command is a string tmux hands to a shell, and CYCLOPS_HOME is the
/// operator's to place: a home under "My Documents" would otherwise split
/// the argument and start claude with a settings path it cannot read. Single
/// quotes take everything literally, so only an embedded quote needs the
/// close-escape-reopen dance.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Put one launch command on each pane the workspace names, in layout
/// order, and mark those panes to run it.
///
/// Layout order is the order `adopt` puts the names on, so the CLI that
/// starts under a name is the one that was listed against it. A pane with
/// no label keeps whatever command it had and stays unmarked: nothing
/// addresses the `ops` dock, its `cyclops ui` is part of the arrangement
/// rather than part of the fleet, and starting it was never asked for. The
/// mark is what keeps the two apart in the same build, so it goes on here,
/// beside the write it belongs to.
fn fill_agents(layout: &mut Layout, commands: &[String]) {
    let mut next = commands.iter();
    for w in &mut layout.windows {
        for r in &mut w.rows {
            for p in &mut r.panes {
                if let Some(label) = p.label.clone() {
                    if let Some(cmd) = next.next() {
                        p.command = Some(cmd.clone());
                        p.launch_now = true;
                        // Namespace for hook sequence counters. Identity comes
                        // from the authenticated socket peer, so a rename does
                        // not require rewriting hook configuration.
                        //
                        // Derived from the label at build time and never
                        // saved, so renaming a pane and rebuilding gives it
                        // the new name rather than the one it had.
                        p.env = vec![("CYCLOPS_AGENT".to_string(), label)];
                    }
                }
            }
        }
    }
}

/// `cyclops start`: have the default workspace open, whatever state the
/// machine was in.
///
/// Steps, in the order they have to happen:
///
/// 1. Resolve which workspace this is, and the layout behind it: the
///    saved file if there is one, else a preset. Then name the CLIs
///    `--agents` asked for, or refuse before anything is built.
/// 2. Build the session if it is not there, and write the workspace file
///    when the layout came from a preset. An existing session is left
///    exactly as it is; this verb is safe to run twice.
/// 3. Make the home usable: the config has to name the session so the
///    daemon watches it, and the detection manifests have to be there or
///    cyclopsd binds no pane and delivers nothing.
/// 4. Make sure a daemon is running, starting one when it is not. After
///    the session exists, so it attaches on its first try.
/// 5. Ask the daemon what it is watching and which panes already answer to
///    a name. Step 6 needs both, so it happens first.
/// 6. Decide whether this workspace may name this session's panes at all.
///    Everything after depends on the answer, because a workspace that
///    may not name the session can neither rename its panes nor say how
///    many agents are in it.
/// 7. Put the names back, when the daemon is watching.
/// 8. Say what is ready, and what is left to do.
pub fn run_start(
    json_out: bool,
    style: &Style,
    asked: Option<&str>,
    asked_session: Option<&str>,
    preset_name: Option<&str>,
    launch: &Launch,
    start_daemon: bool,
) -> i32 {
    let home = cyclops_proto::cyclops_home();
    let settings = match Settings::read(&home) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };

    // 1.
    let name = settings.workspace_name(asked);
    // The session takes the workspace's name unless told otherwise, which
    // is what makes `cyclops start` one word for almost everybody.
    let session = asked_session.unwrap_or(&name).to_string();
    for n in [&name, &session] {
        if let Err(e) = check_name(n) {
            eprintln!("{e}");
            return 2;
        }
    }
    let (mut layout, source, from_preset) = match load(&home, &name) {
        Some(Ok(l)) => (l, format!("workspace {name}"), false),
        Some(Err(e)) => {
            eprintln!("{e}");
            return 1;
        }
        None => {
            let p = preset_name.unwrap_or(FIRST_RUN_PRESET);
            match preset(p) {
                Some(Ok(l)) => (l, format!("preset {p}"), true),
                Some(Err(e)) => {
                    eprintln!("{e}");
                    return 1;
                }
                None => {
                    eprintln!("{}", unknown_preset(p));
                    return 2;
                }
            }
        }
    };
    layout.name = name.clone();

    // The CLIs named on the command line go onto the layout before it is
    // built, so the panes come up running them and the file this run
    // stores carries them. Refusing here costs nothing: no session, no
    // file, no daemon.
    if !launch.agents.is_empty() {
        match resolve_agents(&home, launch.agents, &layout, &source) {
            Ok(commands) => fill_agents(&mut layout, &commands),
            Err(why) => {
                eprintln!("{why}");
                return 2;
            }
        }
    }

    // 2.
    let mut steps: Vec<(String, String)> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let existed = match layout::session_exists(&settings.server, &session) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{}", tmux_trouble(&e));
            return 1;
        }
    };
    let mut pane_ids: Vec<String> = Vec::new();
    if !existed {
        let opts = ApplyOptions {
            // The layout-wide flag is `--launch` alone. What `--agents`
            // starts rides on the panes `fill_agents` marked.
            launch: launch.replays_stored(),
            size: build_size(),
        };
        match layout::apply(&settings.server, &session, &layout, &opts) {
            Ok(ids) => pane_ids = ids,
            Err(e) => {
                eprintln!("{}", tmux_trouble(&e));
                return 1;
            }
        }
        // A workspace becomes a file the moment `start` builds one from a
        // preset. Without that, the next run has nothing but a preset to
        // guess with, and a preset that no longer matches the session
        // cannot name a pane or count an agent. A saved workspace is never
        // overwritten here: it is the thing that was just restored.
        if from_preset {
            if let Err(e) = store(&home, &name, &layout) {
                notes.push(e);
            }
        }
    } else if !launch.agents.is_empty() {
        // Nothing was built, so nothing was started. `start` is safe to
        // run twice by never touching a session that exists, and that rule
        // outranks a flag: killing panes to honor `--agents` would take
        // down whatever is working in them.
        notes.push(agents_not_started(&session));
    }

    // 3.
    let seeded = match prepare_home(&home, &settings, &session) {
        Ok((home_notes, seeded)) => {
            notes.extend(home_notes);
            seeded
        }
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    // And the vendor homes, when the installer's consent is on file: an
    // agent CLI installed after cyclops gets its skill and hook config on
    // this boot instead of never. Quiet unless something was written.
    notes.extend(finish_deferred_wiring(&home));

    // 4. Make sure there is a daemon, starting one if there is not.
    //
    //    Here, and not earlier, because the session now exists: a daemon
    //    that boots with its session already there attaches at once, so
    //    the names below land in this run. An external daemon that started
    //    first is woken explicitly below after this creation succeeds.
    //
    //    A failure to start is a note, not an exit. The workspace is
    //    open and usable at this point; what is lost is naming, and the
    //    lines below already say so when nothing is watching.
    let mut daemon_failed = false;
    if start_daemon {
        match crate::daemon::ensure_running(&home) {
            Ok(crate::daemon::Started::Spawned) => {
                notes.push(format!(
                    "started cyclopsd, logging to {}",
                    crate::daemon::log_path(&home).display()
                ));
            }
            Ok(crate::daemon::Started::AlreadyRunning) => {}
            Err(why) => {
                notes.push(why);
                daemon_failed = true;
            }
        }
    }

    // 5. The registry is the only record of which pane already answers to
    //    which name, and gate 3 below is that record against the
    //    workspace, so the daemon is asked before anything is decided.
    let mut client = daemon();
    let mut watch = match client.as_mut() {
        None => Watch::Down,
        Some(c) => watch_of(c, &session),
    };
    // A daemon started by an external supervisor can already hold this
    // configured session in `NotYet`. `cyclops start` is the creator, so
    // after it has built or restored the tmux session it sends the
    // idempotent availability edge that releases the daemon's wait. Do not
    // send this for `Elsewhere`: that state means the session is not
    // configured, and turning it into a runtime watch would silently change
    // the existing config-hint contract.
    let mut notified_session = false;
    if matches!(watch, Watch::NotYet) {
        if let Some(c) = client.as_mut() {
            if notify_session_available(c, &session) {
                notified_session = true;
                watch = watch_of(c, &session);
            } else {
                // We cannot honestly treat a failed creation notification as
                // a watcher that will catch up on its own.
                watch = Watch::Down;
            }
        }
    }
    // A session built seconds ago is one cyclopsd has not connected to
    // yet, and until it does, no pane can be named. Waiting for it here is
    // what makes one `cyclops start` enough: without this the first run
    // builds the session, names nothing, and a second run is what puts the
    // names on.
    //
    // Only when this run built the session or successfully woke an existing
    // daemon slot, and only with a daemon there to wait for. Every other
    // state is already its own answer.
    if (!existed || notified_session) && matches!(watch, Watch::NotYet | Watch::Watching(_)) {
        if let Some(c) = client.as_mut() {
            watch = wait_for_attach(c, &session, &pane_ids);
        }
    }
    let watching = matches!(watch, Watch::Watching(_));
    let live_labels = match &watch {
        Watch::Watching(labels) => labels.clone(),
        _ => BTreeMap::new(),
    };

    // 6. May this workspace put its names on this session's panes? Three
    //    gates, each preventing the same failure from a different
    //    direction, and a no from any of them renames nothing:
    //
    //    1. The workspace has to be this session's. A preset is a shape
    //       cyclops offers, not one the operator chose for a session that
    //       was already open, and adoption is explicit (docs/guides/panes.md).
    //    2. The live grid has to still have the workspace's shape.
    //    3. Every name already on a pane has to be where the workspace
    //       puts it.
    let mut naming = if !existed {
        // Built from this layout a moment ago, pane by pane.
        Naming::Allowed
    } else if from_preset {
        Naming::Refused(never_built_here(&name, &session))
    } else {
        match layout::capture(&settings.server, &session) {
            Ok(cap) => match first_difference(&cap.layout, &layout) {
                Some(d) => Naming::Refused(shape_changed(&name, &session, &d)),
                None => {
                    pane_ids = cap.pane_ids;
                    Naming::Allowed
                }
            },
            Err(e) => Naming::Refused(shape_unreadable(&session, &tmux_trouble(&e))),
        }
    };
    if matches!(naming, Naming::Allowed) {
        let moved = misplaced_names(&layout, &pane_ids, &live_labels);
        if !moved.is_empty() {
            naming = Naming::Refused(names_moved(&name, &session, &moved));
        }
    }
    let allowed = matches!(naming, Naming::Allowed);
    if let Naming::Refused(why) = naming {
        notes.push(why);
    }

    // 7. Names, and the count the ready line reports.
    //
    //    The daemon's registry is the truth about who is named, so it wins
    //    whenever it can be asked: the number matches `cyclops list`.
    //    Without a daemon the workspace's own names are the best statement
    //    there is, and only while it may name the session at all.
    let mut named: HashSet<String> = live_labels.keys().cloned().collect();
    if watching && allowed {
        if let Some(c) = client.as_mut() {
            let (ids, problems) = adopt(c, &layout, &pane_ids);
            named.extend(ids);
            notes.extend(problems);
        }
    }
    let agents = match (watching, allowed) {
        (true, _) => named.len(),
        (_, true) => layout.agents(),
        (_, false) => 0,
    };
    // A workspace with names in it, and none of them on a pane, looks like
    // the verb did nothing, so it gets a line saying why. Not when a gate
    // already explained itself: that has said it once already.
    //
    // The test is whether the daemon confirmed, NOT what the count says.
    // The count above falls back to the workspace file when nobody could
    // be asked, which is exactly the case this note exists for, so testing
    // the count meant the note never printed and the run looked done.
    if !watching && layout.agents() > 0 && allowed {
        if let Some(why) = unaskable(&watch, &session) {
            notes.push(names_wait_step(&why));
        }
    }
    // 8. Every step here has to be a command that works when it is run.
    //    A step that answers with an error is worse than one step fewer.
    //
    //    No "start the daemon" step: step 4 did that. A daemon still down
    //    here is one that failed to start, and running the same thing by
    //    hand fails the same way; the note step 4 pushed carries the
    //    reason and the log to read.
    // A name is addressable only once cyclopsd holds it, so with no daemon
    // watching, `cyclops send <name>` answers "no pane for ...". What
    // belongs here instead is the command that puts the names on: this
    // verb again, once the daemon is up.
    let addressable = if !watching {
        None
    } else if allowed {
        layout.panes().find_map(|p| p.label.clone())
    } else {
        live_labels.values().next().cloned()
    };
    if addressable.is_none() && layout.agents() > 0 && allowed {
        steps.push((
            start_again(&name, &session),
            "put the workspace's names on the panes".into(),
        ));
    }
    // Offered whenever this shell is outside tmux, not only when this run
    // built the session. Gating it on "just created" dropped the step on
    // the second run of a first setup, which is the run where the operator
    // still has no agent in the pane and most needs to open it.
    if std::env::var_os("TMUX").is_none() {
        steps.push((
            format!("tmux attach -t {session}"),
            "open the workspace and start your agents".into(),
        ));
    }
    if let Some(first) = addressable {
        steps.push((
            format!(
                "cyclops send {first} --subject \"hello\" --summary \"Hello from Cyclops. Reply when you are ready.\""
            ),
            "send the first message".into(),
        ));
    }

    if json_out {
        println!(
            "{}",
            json!({
                "workspace": name,
                "session": session,
                "created": !existed,
                "source": source,
                "agents": agents,
                "describes_session": allowed,
                "confirmed": watching,
                "daemon": watch.word(),
                // What the home holds for pane detection, so a script can
                // branch on a usable install rather than on the copy.
                "manifests": {
                    "dir": seeded.dir.display().to_string(),
                    "written": seeded.written,
                    "kept": seeded.kept,
                },
                "notes": notes,
            })
        );
        // Same rule as the rendered path: a script reading JSON branches
        // on the exit code, so it must not read 0 over a dead daemon.
        return if daemon_failed { 1 } else { 0 };
    }
    println!("{}", ready_line(agents, watching, style));
    for note in &notes {
        println!("  {}", style.dim(note));
    }
    if !steps.is_empty() {
        println!();
        println!("{}", next_steps(&steps, style));
    }
    // The workspace opened, so this is not a failure of the verb. It is
    // still not a run anybody should treat as fine: with no daemon
    // nothing can be named, addressed or recorded, and reporting success
    // is the exact shape that let a broken first run look finished.
    if daemon_failed {
        return 1;
    }
    0
}

/// Everything a run puts in the home directory, and nothing that touches
/// tmux: the config on a first run, and the shipped manifests on every
/// run. Returns the lines to print under the ready line, and what the
/// seed did.
///
/// One home for this because two verbs need it. `cyclops start` runs it on
/// the way to opening a workspace, and `cyclops start --setup-only` runs
/// it alone, which is what the installer calls. A second copy would let
/// the installed home drift from the one a first `start` writes, and that
/// gap is invisible until a message fails to reach a pane.
fn prepare_home(
    home: &Path,
    settings: &Settings,
    session: &str,
) -> Result<(Vec<String>, crate::manifests::Seeded), String> {
    let mut notes: Vec<String> = Vec::new();

    // 1. The config names the session, which is the only reason cyclopsd
    //    watches it. An existing config is never rewritten; it gets a line
    //    saying what to add when it does not name this session.
    if !settings.exists {
        let path = config_path(home);
        if write_first_run_config(home, session)? {
            notes.push(format!("wrote {}", path.display()));
        }
    } else if !settings.watches(session) {
        notes.push(config_hint(&config_path(home), session));
    }

    // 2. Themes, every run for the same reasons as the manifests below.
    //    Quieter stakes: a home without themes renders in built-in colors
    //    rather than delivering nothing, so problems are notes, never an
    //    error.
    let themes = crate::themeseed::seed(home);
    if !themes.written.is_empty() {
        notes.push(crate::themeseed::installed(&themes));
    }
    notes.extend(themes.problems);
    //    The shipped sound cue goes the same way, and a home without it
    //    still has the terminal bell.
    let sounds = crate::soundseed::seed(home);
    if !sounds.written.is_empty() {
        notes.push(crate::soundseed::installed(&sounds));
    }
    notes.extend(sounds.problems);

    // 3. Manifests, every run and not only the first. A home that predates
    //    the seed gets them without a reinstall, and a shipped set that
    //    gains a file reaches an existing home on the next run. Every
    //    existing manifest stays unchanged; a known old shipped seed reports
    //    outdated for manual review.
    let seeded = crate::manifests::seed(home);
    if seeded.none_installed() {
        // The one note that contradicts the ready line above it: with no
        // manifests cyclopsd reads every pane as unknown and no message
        // reaches an agent.
        notes.push(crate::manifests::nothing_installed(&seeded));
    } else {
        if !seeded.written.is_empty() {
            notes.push(crate::manifests::installed(&seeded));
        }
        if !seeded.problems.is_empty() {
            notes.push(crate::manifests::partly_installed(&seeded));
        }
    }

    // 4. The prepared hook configs, every run. An update moves the
    //    binary, and every receipted artifact still naming the old path
    //    reports to nothing until it is re-rendered; this is the repair
    //    the receipt beside each artifact was recorded to allow
    //    (crate::hookset::refresh). Quiet when everything is current.
    notes.extend(crate::hookset::refresh(home).notes());
    Ok((notes, seeded))
}

/// `cyclops start --setup-only`: make the home usable and stop.
///
/// The installer's last step. It writes the same config and manifests a
/// first `cyclops start` would and never opens a session, because an
/// installer that creates tmux sessions behind the operator is a surprise,
/// and because a machine without tmux should still finish installing.
pub fn run_setup(json_out: bool, style: &Style, wire_hooks: bool) -> i32 {
    let home = cyclops_proto::cyclops_home();
    let settings = match Settings::read(&home) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    // The session the config will name. `start` with no arguments opens
    // this same one, so the config this writes is the config that run wants.
    let session = settings.workspace_name(None);

    let (notes, seeded) = match prepare_home(&home, &settings, &session) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    // A home with no manifests cannot deliver to anything, so this verb
    // does not get to call itself done. The installer reads the exit code.
    let usable = !seeded.none_installed();

    // Wire the vendor homes, behind the installer's opt-in. This is the
    // step that makes an install report turn edges and earn verified
    // receipts instead of detecting agents it can never hear from, and it
    // runs on update too because update calls this verb.
    //
    // Never fatal. A hooks.json that cannot be written leaves the agent on
    // the screen-verified tier, which is the tier every agent was on before
    // this existed; refusing to finish an install over it would be trading
    // a working cyclops for none.
    //
    // The consent outlives this run. `wire_vendor_homes` skips any vendor
    // whose directory is not there, and on a machine where cyclops was
    // installed first, that is every vendor; recording the consent is what
    // lets an ordinary boot finish the job once one appears
    // (`finish_deferred_wiring`).
    let consented = wire_hooks && std::env::var_os("CYCLOPS_NO_VENDOR_HOOKS").is_none();
    if consented {
        record_wiring_consent(&home);
    }
    let (wired, skills) = if consented {
        wire_vendor_homes()
    } else {
        (Vec::new(), Vec::new())
    };

    if json_out {
        println!(
            "{}",
            json!({
                "home": home.display().to_string(),
                "session": session,
                "usable": usable,
                "manifests": {
                    "dir": seeded.dir.display().to_string(),
                    "written": seeded.written,
                    "kept": seeded.kept,
                },
                "notes": notes,
                "hooks": wired.iter().map(|w| json!({
                    "vendor": w.vendor,
                    "path": w.path.display().to_string(),
                    "unchanged": w.unchanged,
                    "backup": w.backup.as_ref().map(|b| b.display().to_string()),
                })).collect::<Vec<_>>(),
                // Additive, like every wire change. Machines without the
                // agent CLI report nothing rather than a "no_agent" row,
                // because absence is the ordinary case, not an event.
                "skill": skills.iter()
                    .filter(|s| s.outcome != crate::skillseed::Outcome::NoAgent)
                    .map(|s| json!({
                        "consumer": s.consumer,
                        "path": s.path.display().to_string(),
                        "outcome": crate::skillseed::json_word(&s.outcome),
                        "detail": crate::skillseed::json_detail(&s.outcome),
                    })).collect::<Vec<_>>(),
            })
        );
        return if usable { 0 } else { 1 };
    }
    if !usable {
        for note in &notes {
            eprintln!("{note}");
        }
        return 1;
    }
    // No next steps here, unlike every other verb. Whoever called this
    // owns what comes after it: the installer prints its own list, and a
    // person running it by hand is already past the part that needs one.
    println!(
        "{}",
        style.bold(&format!("{} cyclops is set up", crate::render::check(true)))
    );
    for note in &notes {
        println!("  {}", style.dim(note));
    }
    for note in hook_notes(&wired) {
        println!("  {}", style.dim(&note));
    }
    for s in &skills {
        if let Some(note) = crate::skillseed::note(s) {
            println!("  {}", style.dim(&note));
        }
    }
    0
}

/// Keep hook and skill writes on the same consent path during setup and
/// deferred wiring.
///
/// Both halves gate on consumer homes and preserve bytes Cyclops did not
/// write. A current setup performs no writes.
fn wire_vendor_homes() -> (
    Vec<crate::hookset::WiredVendor>,
    Vec<crate::skillseed::SeededSkill>,
) {
    (wire_installed_vendors(), crate::skillseed::seed())
}

/// Wire every installed vendor's default hook configuration.
///
/// Claude receives both this safe merge for normal direct launches and a
/// per-pane settings file when Cyclops starts the pane itself.
fn wire_installed_vendors() -> Vec<crate::hookset::WiredVendor> {
    let mut out = Vec::new();
    // The consumer catalog is the one list of hook-wired vendors; a vendor
    // added there is wired here without a second list to forget.
    for kind in crate::consumer::SHIPPED.iter().map(|spec| spec.kind) {
        match crate::hookset::wire_vendor(kind) {
            Ok(Some(w)) => out.push(w),
            // Not installed is ordinary and not worth a line.
            Ok(None) => {}
            // Say it and keep going. The operator can still wire this one
            // by hand, and every other vendor still gets its turn.
            Err(cause) => eprintln!("  hooks: {cause}"),
        }
    }
    out
}

/// The file that makes `--wire-hooks` consent outlive the run it was
/// given in.
///
/// Without it the consent lived only in the installer's one invocation:
/// on a machine where cyclops was installed before the agent CLIs, every
/// vendor dir was (rightly) absent, both writes were skipped, and nothing
/// ever came back for them. Only the installer and `cyclops update` pass the
/// flag. While this file exists, every ordinary boot may finish the
/// wiring for a vendor that appeared since ([`finish_deferred_wiring`]).
///
/// [`record_wiring_consent`] is the single writer. Deleting the file
/// withdraws the consent; `CYCLOPS_NO_VENDOR_HOOKS` declines it without
/// deleting anything.
fn consent_marker(home: &Path) -> PathBuf {
    home.join("vendor-wiring-consented")
}

/// Record that `--wire-hooks` was asked for, so later boots may finish
/// the vendor wiring. Never fatal, same as the wiring itself: a marker
/// that cannot be written costs the deferred retry, not the setup, and
/// the line says so.
fn record_wiring_consent(home: &Path) {
    let path = consent_marker(home);
    let text = "Consent to wire vendor homes, recorded by `cyclops start --setup-only \
                --wire-hooks` (the installer's last step).\n\
                While this file exists, every ordinary boot finishes that wiring for \
                agent CLIs installed after cyclops: hook entries in the config each \
                vendor CLI reads on its own, and agent skills at the canonical \
                destinations of installed consumers.\n\
                Delete this file to withdraw the consent; CYCLOPS_NO_VENDOR_HOOKS=1 \
                declines it for one run.\n";
    let result = StateRoot::open_or_create(home).and_then(|root| {
        match root.create_file_once(Path::new("vendor-wiring-consented"), text.as_bytes())? {
            CreateFileOutcome::Created => Ok(()),
            CreateFileOutcome::AlreadyExists => root
                .open_read(Path::new("vendor-wiring-consented"))
                .and_then(|winner| {
                    winner
                        .map(|_| ())
                        .ok_or_else(|| cyclops_state::StateError::Io {
                            path: path.clone(),
                            source: std::io::Error::from(std::io::ErrorKind::NotFound),
                        })
                }),
        }
    });
    if let Err(e) = result {
        eprintln!(
            "  hooks: can't record wiring consent at {}: {e}",
            path.display()
        );
    }
}

/// Finish the install's vendor wiring for agent CLIs that appeared after
/// it ran. Returns lines only when something was actually written, because
/// on every ordinary boot after the first this finds
/// everything current and a note repeated forever is noise.
///
/// The boot path for bare `cyclops` and `cyclops start` calls this; the
/// daemon never does. Writing into vendor homes stays a CLI act, done where
/// a person just ran a command. Without the marker, or with
/// `CYCLOPS_NO_VENDOR_HOOKS` set, no vendor home is even looked at.
pub fn finish_deferred_wiring(home: &Path) -> Vec<String> {
    if !consent_marker(home).is_file() || std::env::var_os("CYCLOPS_NO_VENDOR_HOOKS").is_some() {
        return Vec::new();
    }
    let (wired, skills) = wire_vendor_homes();
    let mut notes: Vec<String> = Vec::new();
    for w in wired.iter().filter(|w| !w.unchanged) {
        notes.push(format!(
            "{} appeared since install: wired cyclops hooks in {}",
            w.vendor,
            w.path.display()
        ));
        notes.extend(kept_backup(w));
    }
    for s in &skills {
        match &s.outcome {
            crate::skillseed::Outcome::Written => notes.push(format!(
                "{} appeared since install: placed the cyclops skill at {}",
                s.consumer,
                s.path.display()
            )),
            // A failure someone can fix, said each boot until it is: the
            // quiet alternative is a consent that silently never lands.
            crate::skillseed::Outcome::Problem(_) => notes.extend(crate::skillseed::note(s)),
            crate::skillseed::Outcome::Kept | crate::skillseed::Outcome::NoAgent => {}
        }
    }
    notes
}

/// One line per vendor whose config this run touched, and one for the
/// backups. A file this project edited that the operator did not edit is
/// exactly the kind of thing they should hear about at the time rather
/// than discover later.
fn hook_notes(wired: &[crate::hookset::WiredVendor]) -> Vec<String> {
    let changed: Vec<&crate::hookset::WiredVendor> =
        wired.iter().filter(|w| !w.unchanged).collect();
    if changed.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = changed
        .iter()
        .map(|w| format!("wired {} hooks in {}", w.vendor, w.path.display()))
        .collect();
    out.extend(changed.iter().filter_map(|w| kept_backup(w)));
    out
}

/// The backup line, one sentence wherever a vendor config was edited.
/// Setup and the deferred boot both print it, because a file this project
/// copied aside is worth hearing about at the time, whichever run did it.
fn kept_backup(w: &crate::hookset::WiredVendor) -> Option<String> {
    w.backup
        .as_ref()
        .map(|b| format!("kept your previous {} config at {}", w.vendor, b.display()))
}

fn write_first_run_config(home: &Path, session: &str) -> Result<bool, String> {
    let path = config_path(home);
    let root =
        StateRoot::open_or_create(home).map_err(|e| format!("create {}: {e}", home.display()))?;
    match root
        .create_file_once(
            Path::new("config.toml"),
            first_run_config(session).as_bytes(),
        )
        .map_err(|e| format!("write {}: {e}", path.display()))?
    {
        CreateFileOutcome::Created => Ok(true),
        CreateFileOutcome::AlreadyExists => root
            .open_read(Path::new("config.toml"))
            .map_err(|e| format!("read {}: {e}", path.display()))?
            .map(|_| false)
            .ok_or_else(|| format!("read {}: file disappeared after creation", path.display())),
    }
}

// ---------------------------------------------------------------------------
// save and restore
// ---------------------------------------------------------------------------

/// Where the names in a save came from.
///
/// A name lives in exactly two places: cyclopsd's registry, and this file.
/// A save that wrote none would leave it in neither, and nothing on the
/// machine would then be able to say what a pane was called. So the file's
/// own names are kept whenever they cannot be read fresh, and the printed
/// line says which of the two happened.
///
/// "Cannot be read fresh" is two situations and not one. The daemon may be
/// down, and the daemon may be up with an empty roster. Neither is the
/// claim that these panes have no names; both are the same silence, and a
/// daemon that restarted before its sessions reattached produces the
/// second one routinely.
#[derive(Debug)]
enum Kept {
    /// The daemon answered with names. Its registry is the roster, and
    /// whatever the file held is last week's copy of it.
    FromDaemon,
    /// Nothing to lose: no file yet, or one with no names in it.
    Nothing,
    /// Carried over from the file, in [`Layout::panes`] order.
    Carried(Vec<Option<String>>),
}

/// The names the workspace file already holds, for a save with nobody able
/// to answer for the roster.
///
/// Refuses rather than writing whenever those names cannot be put back
/// where they were: a file that will not parse, or one describing a
/// different shape. The alternative is a file with the geometry right and
/// the roster gone, and a name that is in neither the registry nor the
/// file is one the operator has to remember by hand.
///
/// `asked_daemon` only picks the next step in the refusal copy: a daemon
/// that is down is started, a daemon with an empty roster is given names.
fn keep_names(
    home: &Path,
    name: &str,
    session: &str,
    cap: &Capture,
    asked_daemon: bool,
) -> Result<Kept, String> {
    let Some(parsed) = load(home, name) else {
        return Ok(Kept::Nothing);
    };
    let old = parsed.map_err(|e| unreadable_workspace(&e))?;
    let held = old.agents();
    if held == 0 {
        return Ok(Kept::Nothing);
    }
    if let Some(d) = first_difference(&cap.layout, &old) {
        return Err(names_have_nowhere(
            &path_of(home, name),
            held,
            session,
            &d,
            asked_daemon,
        ));
    }
    Ok(Kept::Carried(
        old.panes().map(|p| p.label.clone()).collect(),
    ))
}

/// `cyclops workspace save [name]`: write the session as it is now.
///
/// Steps, in the order they have to happen:
///
/// 1. Read the session's shape off tmux. Nothing is written if that fails.
/// 2. Ask cyclopsd for the names. Labels are in its registry and nowhere
///    else; tmux has never heard of them.
/// 3. With nobody able to answer for the roster, keep the names the file
///    already holds. See [`Kept`]: this is the one thing the verb must not
///    get wrong.
/// 4. Write, and say which half was captured. The shape always is; the
///    names only when somebody could answer for them.
pub fn run_save(json_out: bool, style: &Style, name: Option<&str>, session: Option<&str>) -> i32 {
    let home = cyclops_proto::cyclops_home();
    let settings = match Settings::read(&home) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    // The session to read is the default workspace's unless --session
    // says otherwise. The positional argument names the FILE, so
    // `cyclops workspace save review-setup` saves the session you are in
    // under a second name rather than looking for a session by that name.
    let session = session
        .map(str::to_string)
        .unwrap_or_else(|| settings.workspace_name(None));
    let name = name.unwrap_or(&session).to_string();
    for n in [&name, &session] {
        if let Err(e) = check_name(n) {
            eprintln!("{e}");
            return 2;
        }
    }

    // 1.
    let mut capture = match layout::capture(&settings.server, &session) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}", tmux_trouble(&e));
            return 1;
        }
    };
    // 2. Labels are the daemon's registry, not tmux's.
    let (labels, asked_daemon) = match daemon().as_mut() {
        Some(c) => match watch_of(c, &session) {
            Watch::Watching(l) => (l, true),
            _ => (BTreeMap::new(), false),
        },
        None => (BTreeMap::new(), false),
    };
    // 3. The daemon speaks for the names only when it has some. An empty
    //    roster is not the claim "these panes have no names", it is the
    //    daemon having nothing to say about them, and the file is the only
    //    other place those names live.
    let kept = if asked_daemon && !labels.is_empty() {
        Kept::FromDaemon
    } else {
        match keep_names(&home, &name, &session, &capture, asked_daemon) {
            Ok(k) => k,
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
    };
    let ids = capture.pane_ids.clone();
    // Position order on both sides: capture reports pane ids in layout
    // order, and a kept name belongs to the pane it was already on.
    for (i, (pane, id)) in capture
        .layout
        .windows
        .iter_mut()
        .flat_map(|w| w.rows.iter_mut().flat_map(|r| r.panes.iter_mut()))
        .zip(&ids)
        .enumerate()
    {
        pane.label = match &kept {
            Kept::Carried(old) => old.get(i).cloned().flatten(),
            Kept::FromDaemon | Kept::Nothing => labels.get(id).cloned(),
        };
    }
    capture.layout.name = name.clone();

    // 4.
    let path = match store(&home, &name, &capture.layout) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let agents = capture.layout.agents();
    if json_out {
        println!(
            "{}",
            json!({
                "workspace": name,
                "session": session,
                "path": path.display().to_string(),
                "windows": capture.layout.windows.len(),
                "panes": capture.layout.pane_count(),
                "agents": agents,
                "names_from": match kept {
                    Kept::FromDaemon => "daemon",
                    Kept::Nothing => "none",
                    Kept::Carried(_) => "file",
                },
            })
        );
        return 0;
    }
    // The check asks whether a roster stood behind the agent count, which
    // is the same question as "where did these names come from". Reaching
    // the daemon is not enough: names carried out of the file get the
    // light check even when the daemon answered, because the number on
    // this line is the file's.
    println!(
        "{}",
        saved_line(
            &name,
            &path,
            capture.layout.pane_count(),
            agents,
            matches!(kept, Kept::FromDaemon),
            style
        )
    );
    // Which half was captured. Three different outcomes, so three
    // different sentences; a save that read the roster and found names in
    // it needs none of them.
    match kept {
        Kept::FromDaemon if agents == 0 => println!("  {}", style.dim(&no_names(&session, true))),
        Kept::FromDaemon => {}
        Kept::Nothing => println!("  {}", style.dim(&no_names(&session, asked_daemon))),
        Kept::Carried(_) => println!(
            "  {}",
            style.dim(&names_kept(&session, agents, &path, asked_daemon))
        ),
    }
    0
}

/// `cyclops workspace restore [name]`: build a saved workspace again.
pub fn run_restore(
    json_out: bool,
    style: &Style,
    name: Option<&str>,
    session: Option<&str>,
    launch: bool,
) -> i32 {
    let home = cyclops_proto::cyclops_home();
    let settings = match Settings::read(&home) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let name = settings.workspace_name(name);
    let session = session.map(str::to_string).unwrap_or_else(|| name.clone());
    for n in [&name, &session] {
        if let Err(e) = check_name(n) {
            eprintln!("{e}");
            return 2;
        }
    }

    let layout = match load(&home, &name) {
        Some(Ok(l)) => l,
        Some(Err(e)) => {
            eprintln!("{e}");
            return 1;
        }
        None => {
            eprintln!("{}", no_such_workspace(&home, &name));
            return 1;
        }
    };
    let opts = ApplyOptions {
        launch,
        size: build_size(),
    };
    let pane_ids = match layout::apply(&settings.server, &session, &layout, &opts) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("{}", tmux_trouble(&e));
            return 1;
        }
    };

    let mut adopted = 0;
    let mut notes = Vec::new();
    let mut client = daemon();
    let watch = match client.as_mut() {
        None => Watch::Down,
        Some(c) => watch_of(c, &session),
    };
    if let (Watch::Watching(_), Some(c)) = (&watch, client.as_mut()) {
        let (named, problems) = adopt(c, &layout, &pane_ids);
        adopted = named.len();
        notes.extend(problems);
    }
    // A session built a second ago is normally one the daemon has not
    // connected to yet, so this is the ordinary path, not the sad one.
    if adopted == 0 && layout.agents() > 0 {
        if let Some(why) = unaskable(&watch, &session) {
            notes.push(names_wait(&name, &session, &why));
        }
    }

    if json_out {
        println!(
            "{}",
            json!({
                "workspace": name,
                "session": session,
                "panes": layout.pane_count(),
                "agents": layout.agents(),
                "adopted": adopted,
                "daemon": watch.word(),
                "launched": launch,
                "notes": notes,
            })
        );
        return 0;
    }
    println!(
        "{}",
        restored_line(
            &session,
            layout.pane_count(),
            adopted,
            matches!(watch, Watch::Watching(_)),
            style
        )
    );
    for note in &notes {
        println!("  {}", style.dim(note));
    }
    if !launch && layout.panes().any(|p| p.command.is_some()) {
        println!("  {}", style.dim(LAUNCH_HINT));
    }
    0
}

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

/// A workspace saved with no agents in it. The two reasons are different
/// problems: nobody has named these panes, or nobody could be asked.
pub fn no_names(session: &str, asked_daemon: bool) -> String {
    if asked_daemon {
        format!(
            "No agents were named in {session}. Name the panes with cyclops name <pane> <label>, then save again."
        )
    } else {
        format!(
            "cyclopsd isn't watching \"{session}\", so no names were saved. Start it and save again to keep them."
        )
    }
}

/// The save that had nobody to ask for the roster and did not throw it
/// away. Two reasons nobody could answer, so two next steps: start the
/// daemon, or give the panes names it can hold.
pub fn names_kept(session: &str, kept: usize, path: &Path, asked_daemon: bool) -> String {
    let (why, next) = if asked_daemon {
        (
            format!("cyclopsd is watching \"{session}\" but has no names on its roster, so none could be read"),
            "Name the panes with cyclops name <pane> <label>, then save again to capture the roster.".to_string(),
        )
    } else {
        (
            format!("cyclopsd isn't watching \"{session}\", so no names could be read"),
            "Start it and save again to capture the roster.".to_string(),
        )
    };
    format!(
        "{why}. The {} already in {} were kept as they were. {next}",
        count(kept, "name"),
        path.display()
    )
}

/// A save with no roster to read, over a file whose names have nowhere to
/// go.
///
/// Writing here would put the shape on disk correctly and delete a roster
/// that exists nowhere else, so the verb does neither. `asked_daemon`
/// picks the next step, the same two as [`names_kept`].
pub fn names_have_nowhere(
    path: &Path,
    held: usize,
    session: &str,
    difference: &str,
    asked_daemon: bool,
) -> String {
    let next = if asked_daemon {
        "Name the panes with cyclops name <pane> <label> and save again, and the names come from the roster instead of the file."
    } else {
        "Start cyclopsd and save again, and the names come from the roster instead of the file."
    };
    format!(
        "{} holds {} and {session} no longer has its shape: {difference}. Nothing was written, because cyclops can't tell which pane each of those names belongs to now. {next} Or save this shape under another name.",
        path.display(),
        count(held, "name"),
    )
}

/// A save with no daemon, over a file that will not parse. Its names are
/// still readable by a person, so the file stays as it is.
pub fn unreadable_workspace(problem: &str) -> String {
    format!("{problem}. Nothing was written: cyclops won't overwrite a workspace it can't read, in case that file still holds your names.")
}

pub const LAUNCH_HINT: &str =
    "Panes are empty on purpose: a workspace restores structure, not running agents. Add --launch to run the recorded commands.";

/// "1 pane", "3 panes". One helper because these lines count four
/// different things and not one of them may ever read "1 panes".
fn count(n: usize, thing: &str) -> String {
    if n == 1 {
        format!("1 {thing}")
    } else {
        format!("{n} {thing}s")
    }
}

/// The line `cyclops start` exists to print.
///
/// `confirmed` is [`crate::render::check`]'s question, and on all three
/// workspace lines it asks the same thing: did cyclopsd answer for the
/// agent count. Heavy check when the number is its roster, light when it
/// is names read off a file with no daemon running to put them on panes.
/// The pane count needs no such qualifier; cyclops built those panes or
/// just counted them.
pub fn ready_line(agents: usize, confirmed: bool, style: &Style) -> String {
    format!(
        "{} {} {}",
        style.bold(&format!(
            "{} workspace ready",
            crate::render::check(confirmed)
        )),
        style.dim("·"),
        count(agents, "agent")
    )
}

pub fn saved_line(
    name: &str,
    path: &Path,
    panes: usize,
    agents: usize,
    confirmed: bool,
    style: &Style,
) -> String {
    let sep = style.dim("·");
    format!(
        "{} {sep} {name} {sep} {} {sep} {} {sep} {}",
        style.bold(&format!(
            "{} workspace saved",
            crate::render::check(confirmed)
        )),
        count(panes, "pane"),
        count(agents, "agent"),
        style.dim(&path.display().to_string()),
    )
}

pub fn restored_line(
    session: &str,
    panes: usize,
    adopted: usize,
    confirmed: bool,
    style: &Style,
) -> String {
    let sep = style.dim("·");
    format!(
        "{} {sep} {session} {sep} {} {sep} {}",
        style.bold(&format!(
            "{} workspace restored",
            crate::render::check(confirmed)
        )),
        count(panes, "pane"),
        count(adopted, "agent"),
    )
}

/// The guided moment: what to do next, numbered, one action each.
pub fn next_steps(steps: &[(String, String)], style: &Style) -> String {
    let w = steps
        .iter()
        .map(|(cmd, _)| cyclops_ui::grid::display_width(cmd))
        .max()
        .unwrap_or(0);
    let mut out = vec!["Next:".to_string()];
    for (i, (cmd, why)) in steps.iter().enumerate() {
        out.push(format!(
            "  {}  {}  {}",
            style.dim(&format!("{}", i + 1)),
            crate::render::pad(cmd, w),
            style.dim(why)
        ));
    }
    out.join("\n")
}

pub fn no_such_workspace(home: &Path, name: &str) -> String {
    format!(
        "no workspace called \"{name}\" in {}. Save one with: cyclops workspace save {name}",
        home.join("workspaces").display()
    )
}

pub fn unknown_preset(name: &str) -> String {
    format!(
        "no preset called \"{name}\". Cyclops ships: {}.",
        preset_names().join(", ")
    )
}

/// `--agents` named a CLI no manifest describes. The list is every id that
/// can be started, which is the answer to the question that was asked, not
/// every manifest on the machine.
pub fn unknown_agent(id: &str, known: &BTreeMap<String, crate::manifests::Known>) -> String {
    let startable: Vec<&str> = known
        .iter()
        .filter(|(_, m)| m.launch.is_some())
        .map(|(id, _)| id.as_str())
        .collect();
    format!(
        "no agent CLI called \"{id}\". Cyclops can start: {}. Teaching it another one is one file: docs/reference/MANIFESTS.md.",
        startable.join(", ")
    )
}

/// The manifest exists and does not say how to start the CLI. Cyclops does
/// not guess a binary name: the guess fails inside a pane, where the error
/// is a dead shell nobody reads.
pub fn agent_wont_launch(id: &str, path: &Path) -> String {
    format!(
        "the manifest for \"{id}\" doesn't say how to start it. Add launch = \"<command>\" to the [agent] table in {}, then run this again.",
        path.display()
    )
}

/// A comma with nothing on one side of it.
pub fn empty_agent_id() -> String {
    "--agents has an empty id in it. Name one CLI per pane, comma separated: --agents claude,codex."
        .to_string()
}

/// As many CLIs as there are panes to run them in, or nothing runs.
///
/// The line has to carry both numbers and the names behind the second one:
/// "3 doesn't fit 2" is arithmetic, and what the reader needs is which two
/// panes those are and which arrangement takes three.
pub fn agent_count_mismatch(named: usize, source: &str, labels: &[String]) -> String {
    if labels.is_empty() {
        return format!(
            "--agents named {}, and {source} names no panes, so there is nowhere to run them. Label its panes, or start from a preset: {}.",
            count(named, "agent CLI"),
            preset_names().join(", ")
        );
    }
    let fits = match preset_with_agents(named) {
        Some(p) => format!(", or --preset {p}, which has {named}"),
        None => String::new(),
    };
    format!(
        "--agents named {}, and {source} has {} ({}). Name {}{fits}.",
        count(named, "agent CLI"),
        count(labels.len(), "named pane"),
        labels.join(", "),
        labels.len(),
    )
}

/// `--agents` on a session that was already open.
///
/// Not a refusal: the flag is one part of a command that did the rest of
/// its job, and a second `cyclops start` is a thing scripts and people run
/// on purpose. It still cannot pass in silence, because the CLIs the
/// operator asked for are not running.
pub fn agents_not_started(session: &str) -> String {
    format!(
        "{session} was already open, so --agents started nothing: the CLIs it names run in panes cyclops builds. Start them in their panes yourself, or close the session and run this again."
    )
}

/// The session no longer has the workspace's shape, so no pane can be
/// matched to a name with any confidence.
pub fn shape_changed(workspace: &str, session: &str, difference: &str) -> String {
    format!(
        "{session} no longer has the shape of workspace \"{workspace}\": {difference}. Nothing was renamed. Save the session as it is now: {}",
        save_command(workspace, session)
    )
}

/// A name is already on a pane the workspace does not put it on, so the
/// two are describing different sessions whatever their shapes say.
pub fn names_moved(workspace: &str, session: &str, moved: &[String]) -> String {
    format!(
        "the panes in {session} have moved since workspace \"{workspace}\" was saved ({}), so nothing was renamed: a name on the wrong pane sends the next message to the wrong agent. Save the session as it is now: {}",
        moved.join("; "),
        save_command(workspace, session)
    )
}

/// The session was already open and this workspace is a preset cyclops
/// picked, not one the operator saved for it.
///
/// Adoption is explicit (docs/guides/panes.md): cyclops never names a pane
/// because it looks like an agent, and a preset that happens to have the
/// same number of panes is exactly that guess.
pub fn never_built_here(workspace: &str, session: &str) -> String {
    format!(
        "{session} was already open and no workspace called \"{workspace}\" is saved, so nothing was named: cyclops only puts names on panes you named. Capture this session, names and all, with: {}",
        save_command(workspace, session)
    )
}

/// The save that writes THIS session to THAT workspace file. Always
/// spelled with `--session`, because the positional argument names the
/// file and the session would otherwise come from the config's default.
fn save_command(workspace: &str, session: &str) -> String {
    format!("cyclops workspace save {workspace} --session {session}")
}

/// The session's shape cannot be read at all: a nested split, a zoomed
/// pane. The adapter's sentence says which and what to do about it; this
/// adds what it cost.
pub fn shape_unreadable(session: &str, why: &str) -> String {
    format!("{why} Nothing was renamed in {session}.")
}

/// Nothing could be named, and the operator needs to know why and what
/// puts the names on later. `start` is that verb: it never rebuilds a
/// session that exists, it only puts the workspace's names back.
/// Why the daemon could not be asked to name anything, or None when it
/// was asked. A daemon that answered has said its own piece, right down to
/// which name it refused, and a second sentence guessing at another reason
/// would contradict it.
fn unaskable(watch: &Watch, session: &str) -> Option<String> {
    match watch {
        Watch::Down => Some("cyclopsd isn't running".to_string()),
        Watch::Elsewhere => Some(format!("cyclopsd isn't watching \"{session}\"")),
        Watch::NotYet => Some(format!("cyclopsd hasn't connected to \"{session}\" yet")),
        Watch::Watching(_) => None,
    }
}

/// The same fact as [`names_wait`], for a caller whose step list already
/// carries the command. Printing it in both places reads like two things
/// to do when there is one.
fn names_wait_step(why: &str) -> String {
    format!("{why}, so nothing was named yet. The names are in the workspace; the step below puts them on.")
}

fn names_wait(workspace: &str, session: &str, why: &str) -> String {
    format!(
        "{why}, so nothing was named. The names are in the workspace. Put them on with: {}",
        start_again(workspace, session)
    )
}

/// The shortest `cyclops start` that names this workspace's panes in this
/// session.
fn start_again(workspace: &str, session: &str) -> String {
    if workspace == session {
        "cyclops start".to_string()
    } else {
        format!("cyclops start --workspace {workspace} --session {session}")
    }
}

/// tmux said no. The adapter's layout errors are already whole sentences
/// aimed at a human; everything else gets named as a tmux problem.
pub fn tmux_trouble(e: &cyclops_tmux::TmuxError) -> String {
    match e {
        cyclops_tmux::TmuxError::Layout(msg) => msg.clone(),
        cyclops_tmux::TmuxError::Spawn(cause) => {
            format!("can't run tmux: {cause}. Cyclops needs tmux 3.2 or newer on your PATH.")
        }
        other => format!("tmux refused: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_preset_parses_and_validates() {
        for name in preset_names() {
            let layout = preset(name).expect("preset exists").expect("preset parses");
            assert_eq!(layout.name, name, "preset names itself");
            assert!(layout.about.is_some(), "{name} says what it is for");
            assert!(!layout.windows.is_empty());
        }
        assert!(preset("nope").is_none());
    }

    /// The goldens. A preset is a design decision, so the shape is pinned
    /// here rather than described: an edit to one of these files that
    /// changes what a user gets has to change this test too.
    #[test]
    fn preset_goldens() {
        let solo = preset("solo").unwrap().unwrap();
        assert_eq!(solo.pane_count(), 1);
        assert_eq!(solo.agents(), 1);
        assert_eq!(solo.windows[0].rows.len(), 1);
        assert_eq!(
            solo.panes().map(|p| p.label.clone()).collect::<Vec<_>>(),
            vec![Some("implementer".to_string())]
        );

        let duo = preset("duo").unwrap().unwrap();
        assert_eq!(duo.pane_count(), 2);
        assert_eq!(duo.agents(), 2);
        assert_eq!(duo.windows[0].rows.len(), 1);
        assert_eq!(
            duo.windows[0].rows[0]
                .panes
                .iter()
                .map(|p| p.ratio)
                .collect::<Vec<_>>(),
            vec![0.5, 0.5]
        );

        let quad = preset("quad").unwrap().unwrap();
        assert_eq!(quad.pane_count(), 4);
        assert_eq!(quad.agents(), 4);
        assert_eq!(quad.windows[0].rows.len(), 2, "quad is two rows of two");
        assert!(quad.windows[0].rows.iter().all(|r| r.panes.len() == 2));

        // ops is the one with a decision in it: three agents across the
        // top and the stream docked under them at 30%, unlabelled because
        // nothing addresses the stream.
        let ops = preset("ops").unwrap().unwrap();
        assert_eq!(ops.pane_count(), 4);
        assert_eq!(ops.agents(), 3);
        let rows = &ops.windows[0].rows;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ratio, 0.70);
        assert_eq!(rows[1].ratio, 0.30);
        assert_eq!(
            rows[0]
                .panes
                .iter()
                .map(|p| p.label.clone().unwrap())
                .collect::<Vec<_>>(),
            vec!["implementer", "reviewer", "tests"]
        );
        assert_eq!(rows[1].panes.len(), 1);
        assert_eq!(rows[1].panes[0].label, None, "the dock is not an agent");
        assert_eq!(rows[1].panes[0].command.as_deref(), Some("cyclops ui"));
    }

    /// The ladder: each preset is the one before it plus a pane, and the
    /// names carry over. This keeps the presets one pattern to learn; four
    /// unrelated arrangements would be four things to learn instead of one.
    #[test]
    fn the_presets_are_a_ladder() {
        let labels = |name: &str| -> Vec<String> {
            preset(name)
                .unwrap()
                .unwrap()
                .panes()
                .filter_map(|p| p.label.clone())
                .collect()
        };
        assert_eq!(labels("solo"), ["implementer"]);
        assert_eq!(labels("duo"), ["implementer", "reviewer"]);
        assert_eq!(labels("quad"), ["implementer", "reviewer", "tests", "docs"]);
        assert_eq!(labels("ops"), ["implementer", "reviewer", "tests"]);
    }

    /// `--agents` fills the panes the workspace names, in the order the
    /// names are in, and leaves everything else alone. The `ops` dock is
    /// the case that matters: it carries a command of its own and no
    /// label, so a fleet must neither run in it nor push it out of line.
    #[test]
    fn agents_fill_the_named_panes_in_order_and_leave_the_dock_alone() {
        let mut ops = preset("ops").unwrap().unwrap();
        fill_agents(
            &mut ops,
            &["claude".to_string(), "codex".to_string(), "agy".to_string()],
        );
        let panes: Vec<(Option<String>, Option<String>)> = ops
            .panes()
            .map(|p| (p.label.clone(), p.command.clone()))
            .collect();
        assert_eq!(
            panes,
            vec![
                (Some("implementer".into()), Some("claude".into())),
                (Some("reviewer".into()), Some("codex".into())),
                (Some("tests".into()), Some("agy".into())),
                (None, Some("cyclops ui".into())),
            ]
        );
        // Still a layout cyclops will build.
        ops.validate().expect("filled layout is applicable");
    }

    /// The ids `--agents` accepts are manifest ids, and each one becomes
    /// the command its manifest names. Two panes may run the same CLI:
    /// that is a fleet of two claudes, not a mistake.
    ///
    /// claude also leaves here already wired. Its manifest names a
    /// settings flag, so each pane gets its OWN hook config, keyed by the
    /// label that pane answers to: the hook command embeds `--agent
    /// <label>`, so two claudes sharing one file would report each other's
    /// turns. codex names no flag and passes through untouched.
    #[test]
    fn agent_ids_become_the_launch_commands_their_manifests_name() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-agents-ok");
        let _ = std::fs::remove_dir_all(&home);
        let duo = preset("duo").unwrap().unwrap();
        let labels: Vec<String> = duo.panes().filter_map(|p| p.label.clone()).collect();

        let ids = ["claude".to_string(), "claude".to_string()];
        let got = resolve_agents(&home, &ids, &duo, "preset duo").unwrap();
        for (cmd, label) in got.iter().zip(&labels) {
            let want = crate::hookset::hooks_root_in(&home)
                .join("claude")
                .join(label)
                .join("settings.json");
            assert_eq!(cmd, &format!("claude --settings '{}'", want.display()));
            // Wiring is a file that exists, not a path that reads well.
            assert!(want.is_file(), "{} was not written", want.display());
        }

        // A shell that kept the spaces after the commas passed the same
        // list, and cyclops reads it as one. codex discovers its hooks from
        // $CODEX_HOME instead of a flag, so its command is the bare CLI.
        let spaced = ["claude".to_string(), " codex".to_string()];
        let got = resolve_agents(&home, &spaced, &duo, "preset duo").unwrap();
        assert!(got[0].starts_with("claude --settings '"));
        // codex reads a single $CODEX_HOME/hooks.json shared by every pane,
        // so it takes no flag and its command is the bare CLI. Its identity
        // rides in the pane environment instead, which is what lets that one
        // shared file carry no --agent.
        assert_eq!(got[1], "codex");

        // Nothing was written outside the home it was handed.
        assert!(crate::hookset::hooks_root_in(&home).starts_with(&home));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Every started pane says who its hooks report as, and says it in the
    /// environment rather than in the command.
    ///
    /// This is what makes one shared vendor config correct for a fleet:
    /// codex reads a single $CODEX_HOME/hooks.json and agy a single
    /// ~/.agents/hooks.json, so the label cannot live in either file. A pane
    /// nobody named gets nothing, because nothing addresses it.
    #[test]
    fn every_started_pane_names_who_its_hooks_report_as() {
        let mut duo = preset("duo").unwrap().unwrap();
        let labels: Vec<String> = duo.panes().filter_map(|p| p.label.clone()).collect();
        fill_agents(&mut duo, &["claude".to_string(), "codex".to_string()]);

        for (pane, label) in duo.panes().filter(|p| p.label.is_some()).zip(&labels) {
            assert!(pane.launch_now, "{label} was not started");
            assert_eq!(
                pane.env,
                vec![("CYCLOPS_AGENT".to_string(), label.clone())],
                "{label} would report under the wrong name"
            );
            // The identity is NOT in the command; that is the whole point.
            assert!(!pane
                .command
                .as_deref()
                .unwrap_or("")
                .contains("CYCLOPS_AGENT"));
        }

        // An unlabeled pane is part of the arrangement, not the fleet.
        for pane in duo.panes().filter(|p| p.label.is_none()) {
            assert!(pane.env.is_empty());
        }
    }

    /// A home under a path with a space still starts a wired claude. The
    /// command is a string a shell splits, so an unquoted settings path
    /// would hand claude a truncated argument and it would start with no
    /// hooks and no complaint.
    #[test]
    fn a_home_with_a_space_still_quotes_into_one_argument() {
        let base = cyclops_proto::scratch::scratch_dir("cyc agents space");
        let _ = std::fs::remove_dir_all(&base);
        let duo = preset("duo").unwrap().unwrap();
        let labels: Vec<String> = duo.panes().filter_map(|p| p.label.clone()).collect();
        let ids = ["claude".to_string(), "claude".to_string()];
        let got = resolve_agents(&base, &ids, &duo, "preset duo").unwrap();
        for (cmd, label) in got.iter().zip(&labels) {
            let _ = label;
            let path = cmd.strip_prefix("claude --settings ").expect("wired");
            assert!(path.starts_with('\'') && path.ends_with('\''));
            assert!(Path::new(path.trim_matches('\'')).is_file());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Every way `--agents` is refused, and the sentence each one owes the
    /// reader. All four happen before anything is built.
    #[test]
    fn agents_are_refused_by_id_by_launch_and_by_count() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-agents-no");
        let _ = std::fs::remove_dir_all(&home);
        let duo = preset("duo").unwrap().unwrap();
        let refuse = |ids: &[&str], layout: &Layout, source: &str| -> String {
            let ids: Vec<String> = ids.iter().map(|s| (*s).to_string()).collect();
            resolve_agents(&home, &ids, layout, source).expect_err("refused")
        };

        // A typo names no manifest, and the answer is the list of ids that
        // would have worked.
        let why = refuse(&["cluade", "codex"], &duo, "preset duo");
        assert!(why.starts_with("no agent CLI called \"cluade\""), "{why}");
        assert!(why.contains("agy, claude, codex, cursor"), "{why}");

        // An empty id: a stray comma, not a CLI called "".
        assert!(refuse(&["claude", ""], &duo, "preset duo").contains("empty id"));

        // Too few and too many are the same refusal, and it names what
        // would fit rather than only what does not.
        let few = refuse(&["claude"], &duo, "preset duo");
        assert_eq!(
            few,
            "--agents named 1 agent CLI, and preset duo has 2 named panes (implementer, reviewer). Name 2, or --preset solo, which has 1."
        );
        let many = refuse(&["claude", "codex", "agy"], &duo, "preset duo");
        assert!(many.contains("--preset ops, which has 3"), "{many}");
        // Nothing ships with five named panes, so there is nothing to
        // suggest and the line stops rather than inventing one.
        let five = ["claude"; 5];
        let none = refuse(&five, &duo, "preset duo");
        assert!(none.ends_with("Name 2."), "{none}");

        // A manifest that detects a CLI without saying how to start it.
        // The line names the file, because that is where the fix goes.
        let dir = crate::manifests::dir(&home);
        std::fs::create_dir_all(&dir).expect("manifests dir");
        std::fs::write(
            dir.join("mine.toml"),
            "[agent]\nid = \"mine\"\ndisplay_name = \"Mine\"\n",
        )
        .expect("write mine");
        let solo = preset("solo").unwrap().unwrap();
        let quiet = refuse(&["mine"], &solo, "preset solo");
        assert!(quiet.contains("doesn't say how to start it"), "{quiet}");
        assert!(quiet.contains("mine.toml"), "{quiet}");
        // And it is not offered as one of the ids that would work.
        let listed = refuse(&["nope"], &solo, "preset solo");
        assert!(!listed.contains("mine"), "{listed}");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A workspace that names no panes has nowhere to put a fleet, and the
    /// arithmetic line would read "0 named panes ()".
    #[test]
    fn agents_over_a_workspace_with_no_names_says_there_is_nowhere_to_run() {
        let why = agent_count_mismatch(2, "workspace main", &[]);
        assert!(why.contains("names no panes"), "{why}");
        assert!(why.contains("solo, duo, quad, ops"), "{why}");
    }

    /// Naming CLIs at the keyboard runs them; a file naming them does not.
    /// Both halves of the rule in one place, and the split that lets them
    /// both hold in one run.
    #[test]
    fn naming_agents_runs_the_commands_and_a_bare_start_does_not() {
        let names = ["claude".to_string()];
        // `--agents` replays nothing off disk. The fleet it starts is
        // carried by the panes it wrote, one at a time.
        assert!(!Launch {
            stored: false,
            agents: &names
        }
        .replays_stored());
        assert!(Launch {
            stored: true,
            agents: &[]
        }
        .replays_stored());
        assert!(!Launch {
            stored: false,
            agents: &[]
        }
        .replays_stored());

        // And the panes it wrote are the named ones. The `ops` dock has a
        // command of its own and stays unmarked, so a build under
        // `--agents` leaves it at a shell.
        let mut ops = preset("ops").unwrap().unwrap();
        fill_agents(
            &mut ops,
            &["claude".to_string(), "codex".to_string(), "agy".to_string()],
        );
        assert_eq!(
            ops.panes().map(|p| p.launch_now).collect::<Vec<_>>(),
            vec![true, true, true, false]
        );
    }

    #[test]
    fn a_saved_workspace_round_trips_through_the_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = cyclops_proto::scratch::scratch_dir("cyc-ws-file");
        let _ = std::fs::remove_dir_all(&dir);
        let layout = preset("ops").unwrap().unwrap();
        let path = store(&dir, "saved", &layout).expect("store");
        assert!(path.ends_with("workspaces/saved.toml"));
        let back = load(&dir, "saved").expect("file is there").expect("parses");
        assert_eq!(back, layout);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(load(&dir, "absent").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_linked_workspace_directory_is_refused_without_touching_its_target() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let home = cyclops_proto::scratch::scratch_dir("cyc-ws-link-home");
        let external = cyclops_proto::scratch::scratch_dir("cyc-ws-link-external");
        for path in [&home, &external] {
            let _ = std::fs::remove_dir_all(path);
            std::fs::create_dir_all(path).unwrap();
        }
        let target = external.join("saved.toml");
        std::fs::write(&target, b"external\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&external, home.join("workspaces")).unwrap();

        let error = store(&home, "saved", &preset("solo").unwrap().unwrap()).unwrap_err();

        assert!(
            error.contains("linked or non-directory component"),
            "{error}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"external\n");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&external);
    }

    /// A layout as if it had just been read off a live session.
    fn as_captured(layout: &Layout) -> Capture {
        let mut live = layout.clone();
        // tmux has never heard of a label, so a capture never carries one.
        for w in &mut live.windows {
            for r in &mut w.rows {
                for p in &mut r.panes {
                    p.label = None;
                }
            }
        }
        let pane_ids = (0..live.pane_count()).map(|i| format!("%{i}")).collect();
        Capture {
            layout: live,
            pane_ids,
        }
    }

    /// `save` with no daemon must not delete the roster.
    ///
    /// A name lives in the registry and in this file. The registry is the
    /// unreachable one here, so writing the file with no names in it would
    /// leave the name nowhere, and nothing on the machine could say what
    /// the pane was called.
    #[test]
    fn a_save_with_no_daemon_keeps_the_names_the_file_already_holds() {
        let dir = cyclops_proto::scratch::scratch_dir("cyc-ws-keep");
        let _ = std::fs::remove_dir_all(&dir);
        let duo = preset("duo").unwrap().unwrap();
        let cap = as_captured(&duo);

        // No file yet: nothing to lose.
        assert!(matches!(
            keep_names(&dir, "duo", "duo", &cap, false).unwrap(),
            Kept::Nothing
        ));

        // A file with names, over a session that still has its shape.
        store(&dir, "duo", &duo).expect("store");
        match keep_names(&dir, "duo", "duo", &cap, false).unwrap() {
            Kept::Carried(v) => assert_eq!(
                v,
                vec![
                    Some("implementer".to_string()),
                    Some("reviewer".to_string())
                ]
            ),
            _ => panic!("the names in the file are the ones to keep"),
        }

        // The same file over a session that no longer has that shape.
        // The names have nowhere to go, so nothing is written at all.
        let mut split = duo.clone();
        let extra = split.windows[0].rows[0].panes[0].clone();
        split.windows[0].rows[0].panes.push(extra);
        let e = keep_names(&dir, "duo", "duo", &as_captured(&split), false).unwrap_err();
        assert!(e.contains("holds 2 names"), "{e}");
        assert!(e.contains("Nothing was written"), "{e}");
        assert!(e.contains("Start cyclopsd"), "{e}");

        // A file with no names in it is nothing to lose either.
        let mut bare = duo.clone();
        for p in bare.windows[0].rows[0].panes.iter_mut() {
            p.label = None;
        }
        store(&dir, "bare", &bare).expect("store");
        assert!(matches!(
            keep_names(&dir, "bare", "duo", &cap, false).unwrap(),
            Kept::Nothing
        ));

        // A file that will not parse still holds its names as text a
        // person can read, so it is not overwritten either.
        std::fs::write(path_of(&dir, "broken"), "this is not toml").expect("write");
        let e = keep_names(&dir, "broken", "duo", &cap, false).unwrap_err();
        assert!(
            e.contains("won't overwrite a workspace it can't read"),
            "{e}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same rule reached the other way: a daemon that IS watching and
    /// holds no names. Its silence is not testimony that the panes are
    /// unnamed, so the file's own names survive, and the two sentences a
    /// person reads name the right cause and the right next step.
    #[test]
    fn an_empty_roster_is_silence_and_not_a_claim_that_the_panes_are_unnamed() {
        let dir = cyclops_proto::scratch::scratch_dir("cyc-ws-empty");
        let _ = std::fs::remove_dir_all(&dir);
        let duo = preset("duo").unwrap().unwrap();
        let cap = as_captured(&duo);
        store(&dir, "duo", &duo).expect("store");

        // Same names carried, whether the daemon was down or empty.
        match keep_names(&dir, "duo", "duo", &cap, true).unwrap() {
            Kept::Carried(v) => assert_eq!(
                v,
                vec![
                    Some("implementer".to_string()),
                    Some("reviewer".to_string())
                ]
            ),
            _ => panic!("an empty roster must not delete the file's names"),
        }

        // The copy differs where the cause and the fix differ.
        let path = path_of(&dir, "duo");
        let empty = names_kept("duo", 2, &path, true);
        assert!(empty.contains("has no names on its roster"), "{empty}");
        assert!(empty.contains("The 2 names already in"), "{empty}");
        assert!(empty.contains("were kept as they were"), "{empty}");
        assert!(empty.contains("cyclops name <pane> <label>"), "{empty}");
        assert!(!empty.contains("isn't watching"), "{empty}");
        let down = names_kept("duo", 2, &path, false);
        assert!(down.contains("cyclopsd isn't watching \"duo\""), "{down}");
        assert!(down.contains("Start it and save again"), "{down}");

        // And the refusal it can also land on says the same two things.
        let mut split = duo.clone();
        let extra = split.windows[0].rows[0].panes[0].clone();
        split.windows[0].rows[0].panes.push(extra);
        let e = keep_names(&dir, "duo", "duo", &as_captured(&split), true).unwrap_err();
        assert!(e.contains("Nothing was written"), "{e}");
        assert!(e.contains("cyclops name <pane> <label>"), "{e}");
        assert!(!e.contains("Start cyclopsd"), "{e}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_workspace_names_are_refused() {
        for bad in ["", "../escape", "a/b", "main:0", "main.1", "with space"] {
            assert!(check_name(bad).is_err(), "{bad:?} should be refused");
        }
        for good in ["main", "review-2", "my_workspace", "ops"] {
            assert!(check_name(good).is_ok(), "{good:?} should be allowed");
        }
    }

    #[test]
    fn broken_layout_files_say_what_is_wrong() {
        assert!(parse("this is not toml").is_err());
        let dup = "name = \"x\"\n[[windows]]\nname = \"x\"\n[[windows.rows]]\nratio = 1.0\n[[windows.rows.panes]]\nlabel = \"a\"\nratio = 1.0\n[[windows.rows.panes]]\nlabel = \"a\"\nratio = 1.0\n";
        assert!(parse(dup).unwrap_err().contains("used by two panes"));
    }

    #[test]
    fn the_workspace_name_falls_back_in_order() {
        let mut s = Settings {
            sessions: vec![],
            default_workspace: None,
            server: Server::default(),
            exists: false,
        };
        assert_eq!(s.workspace_name(None), "main");
        s.sessions = vec!["watched".into()];
        assert_eq!(s.workspace_name(None), "watched");
        s.default_workspace = Some("configured".into());
        assert_eq!(s.workspace_name(None), "configured");
        assert_eq!(s.workspace_name(Some("asked")), "asked");
    }

    #[test]
    fn the_workspace_launcher_falls_back_when_default_workspace_is_not_a_string() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-default-workspace-owner");
        std::fs::create_dir_all(&home).expect("create scratch home");
        std::fs::write(
            config_path(&home),
            "sessions = [\"watched\"]\ndefault_workspace = 3\n",
        )
        .expect("write config");

        assert_eq!(
            Settings::read(&home)
                .expect("valid coordinator settings")
                .workspace_name(None),
            "watched"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn coordinator_settings_keep_a_valid_nondefault_tmux_server() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-settings-configured-server");
        let _ = std::fs::remove_dir_all(&home);
        StateRoot::open_or_create(&home)
            .expect("create safe home")
            .replace_file(
                Path::new("config.toml"),
                b"sessions = [\"main\"]\ntmux_socket = \"configured\"\ntmux_config = \"/dev/null\"\n",
            )
            .expect("write safe config");

        let settings = Settings::read(&home).expect("valid settings");
        assert_eq!(settings.sessions, ["main"]);
        assert_eq!(settings.server.socket.as_deref(), Some("configured"));
        assert_eq!(
            settings.server.config_file.as_deref(),
            Some(Path::new("/dev/null"))
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn malformed_coordinator_config_refuses_before_workspace_can_default_tmux() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-settings-malformed-config");
        let _ = std::fs::remove_dir_all(&home);
        StateRoot::open_or_create(&home)
            .expect("create safe home")
            .replace_file(Path::new("config.toml"), b"tmux_socket = [")
            .expect("write malformed config");

        assert!(Settings::read(&home).is_err());

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn first_run_config_names_the_session_twice() {
        let text = first_run_config("main");
        let table: toml::Table = text.parse().expect("valid toml");
        assert_eq!(table["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(table["sessions"][0].as_str(), Some("main"));
        assert_eq!(table["default_workspace"].as_str(), Some("main"));
    }

    #[test]
    fn first_run_config_and_consent_are_created_once_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = cyclops_proto::scratch::scratch_dir("cyc-first-run-owner-only");
        let _ = std::fs::remove_dir_all(&home);

        assert!(write_first_run_config(&home, "main").unwrap());
        assert!(!write_first_run_config(&home, "other").unwrap());
        assert!(std::fs::read_to_string(config_path(&home))
            .unwrap()
            .contains("main"));
        assert_eq!(
            std::fs::metadata(config_path(&home))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        record_wiring_consent(&home);
        assert_eq!(
            std::fs::metadata(consent_marker(&home))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn first_run_create_once_writers_refuse_linked_winners() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let home = cyclops_proto::scratch::scratch_dir("cyc-first-run-link-home");
        let external = cyclops_proto::scratch::scratch_dir("cyc-first-run-link-external");
        for path in [&home, &external] {
            let _ = std::fs::remove_dir_all(path);
            std::fs::create_dir_all(path).unwrap();
        }
        let config_target = external.join("config.toml");
        let consent_target = external.join("consent");
        std::fs::write(&config_target, b"external config\n").unwrap();
        std::fs::write(&consent_target, b"external consent\n").unwrap();
        for path in [&config_target, &consent_target] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        symlink(&config_target, config_path(&home)).unwrap();
        symlink(&consent_target, consent_marker(&home)).unwrap();

        assert!(write_first_run_config(&home, "main").is_err());
        record_wiring_consent(&home);

        assert_eq!(std::fs::read(&config_target).unwrap(), b"external config\n");
        assert_eq!(
            std::fs::read(&consent_target).unwrap(),
            b"external consent\n"
        );
        for path in [&config_target, &consent_target] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&external);
    }

    #[test]
    fn the_ready_line_counts_and_pluralizes() {
        let plain = Style::none();
        assert_eq!(ready_line(3, true, &plain), "✔ workspace ready · 3 agents");
        assert_eq!(ready_line(1, true, &plain), "✔ workspace ready · 1 agent");
        assert_eq!(ready_line(0, true, &plain), "✔ workspace ready · 0 agents");
    }

    /// The check weight is [`crate::render::check`]'s rule, and these
    /// three lines are the only places this file prints one. The same
    /// count reads differently depending on who answered for it: cyclopsd
    /// is a roster, a file with no daemon behind it is an intention.
    #[test]
    fn the_workspace_lines_wear_the_check_the_evidence_earns() {
        let plain = Style::none();
        let path = Path::new("/x/main.toml");
        assert_eq!(ready_line(3, false, &plain), "✓ workspace ready · 3 agents");
        assert_eq!(
            saved_line("main", path, 4, 3, true, &plain),
            "✔ workspace saved · main · 4 panes · 3 agents · /x/main.toml"
        );
        assert_eq!(
            saved_line("main", path, 4, 3, false, &plain),
            "✓ workspace saved · main · 4 panes · 3 agents · /x/main.toml"
        );
        assert_eq!(
            restored_line("review", 4, 3, true, &plain),
            "✔ workspace restored · review · 4 panes · 3 agents"
        );
        assert_eq!(
            restored_line("review", 4, 0, false, &plain),
            "✓ workspace restored · review · 4 panes · 0 agents"
        );
    }

    /// The cardinal rule, at the level the decision is made.
    ///
    /// `ops` and `quad` are both four panes and are not the same session:
    /// `ops` is three across with a dock under them, `quad` is two by two.
    /// A check that counts panes says these match, and mapping either
    /// layout's names onto the other renames every agent onto a pane it
    /// does not own.
    #[test]
    fn pane_count_is_not_a_shape() {
        let ops = preset("ops").unwrap().unwrap();
        let quad = preset("quad").unwrap().unwrap();
        assert_eq!(ops.pane_count(), quad.pane_count(), "the trap: same count");
        let d = first_difference(&quad, &ops).expect("a quad is not an ops");
        assert!(d.contains("row 1"), "{d}");
        assert!(d.contains("has 2 panes"), "{d}");
        assert!(d.contains("describes 3 panes"), "{d}");

        // A workspace always describes itself, and ratios are not shape:
        // resizing a pane moves no agent.
        assert_eq!(first_difference(&ops, &ops), None);
        let mut resized = ops.clone();
        resized.windows[0].rows[0].ratio = 0.42;
        resized.windows[0].rows[0].panes[0].ratio = 0.9;
        assert_eq!(first_difference(&resized, &ops), None);
    }

    /// The other two levels of the same check, and the sentences a person
    /// reads when one of them fires.
    #[test]
    fn a_changed_grid_says_where_it_changed() {
        let duo = preset("duo").unwrap().unwrap();

        let mut split = duo.clone();
        let extra = duo.windows[0].rows[0].panes[0].clone();
        split.windows[0].rows[0].panes.push(extra);
        let d = first_difference(&split, &duo).expect("three panes is not two");
        assert!(
            d.contains("has 3 panes and the workspace describes 2 panes"),
            "{d}"
        );

        let mut stacked = duo.clone();
        stacked.windows[0].rows.push(duo.windows[0].rows[0].clone());
        let d = first_difference(&stacked, &duo).expect("two rows is not one");
        assert!(
            d.contains("has 2 rows and the workspace describes 1 row"),
            "{d}"
        );

        let mut two_windows = duo.clone();
        two_windows.windows.push(duo.windows[0].clone());
        let d = first_difference(&two_windows, &duo).expect("two windows is not one");
        assert!(
            d.contains("has 2 windows and the workspace describes 1 window"),
            "{d}"
        );
    }

    /// The check the grid cannot make. `swap-pane` leaves the shape
    /// identical and takes the names with it, so the only thing left that
    /// identifies a pane is the name it already answers to.
    #[test]
    fn a_name_sitting_on_the_wrong_pane_is_a_session_that_moved() {
        let duo = preset("duo").unwrap().unwrap();
        let ids = vec!["%0".to_string(), "%1".to_string()];
        let live = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect()
        };

        // Nothing named yet, and names already where the file puts them:
        // both are this workspace's session.
        assert!(misplaced_names(&duo, &ids, &BTreeMap::new()).is_empty());
        assert!(misplaced_names(
            &duo,
            &ids,
            &live(&[("%0", "implementer"), ("%1", "reviewer")])
        )
        .is_empty());

        // Swapped: naming by position would hand each agent the other's
        // address.
        let moved = misplaced_names(
            &duo,
            &ids,
            &live(&[("%0", "reviewer"), ("%1", "implementer")]),
        );
        assert_eq!(moved.len(), 2, "{moved:?}");
        assert!(moved[0].contains("%0 answers to \"reviewer\""), "{moved:?}");
        assert!(moved[0].contains("puts \"implementer\" there"), "{moved:?}");

        // A name on a pane the workspace leaves unnamed is the same
        // problem: the ops dock is not an agent, so a roster name there
        // means the panes moved under it.
        let ops = preset("ops").unwrap().unwrap();
        let four: Vec<String> = ["%0", "%1", "%2", "%3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let moved = misplaced_names(
            &ops,
            &four,
            &live(&[("%0", "implementer"), ("%1", "reviewer"), ("%3", "tests")]),
        );
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert!(moved[0].contains("%3 answers to \"tests\""), "{moved:?}");
        assert!(moved[0].contains("names no agent there"), "{moved:?}");
    }

    #[test]
    fn the_three_steps_are_a_grid() {
        let plain = Style::none();
        let steps = vec![
            ("cyclopsd &".to_string(), "start the daemon".to_string()),
            (
                "tmux attach -t main".to_string(),
                "open the workspace and start your agents".to_string(),
            ),
            (
                "cyclops send implementer --subject \"hello\" --summary \"Hello from Cyclops. Reply when you are ready.\"".to_string(),
                "send the first message".to_string(),
            ),
        ];
        let expected = "Next:\n\
                        \x20 1  cyclopsd &                                                                                            start the daemon\n\
                        \x20 2  tmux attach -t main                                                                                   open the workspace and start your agents\n\
                        \x20 3  cyclops send implementer --subject \"hello\" --summary \"Hello from Cyclops. Reply when you are ready.\"  send the first message";
        assert_eq!(next_steps(&steps, &plain), expected);
    }
}
