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

use cyclops_tmux::layout::{self, ApplyOptions, Capture, Layout, Server};
use serde_json::{json, Value};

use crate::client::{Client, ClientError};
use crate::style::Style;

/// The shipped presets, in ladder order: each is the one before it plus a
/// pane. Compiled in rather than read from disk so `cyclops start` works
/// on an install that is nothing but two binaries.
const PRESETS: &[(&str, &str)] = &[
    ("solo", include_str!("../../../layouts/solo.toml")),
    ("duo", include_str!("../../../layouts/duo.toml")),
    ("quad", include_str!("../../../layouts/quad.toml")),
    ("ops", include_str!("../../../layouts/ops.toml")),
];

/// Fallback workspace name when neither the config nor the command line
/// names one. Matches the session name every doc page uses.
const DEFAULT_WORKSPACE: &str = "main";

/// Preset `cyclops start` builds when there is nothing saved. The ladder
/// starts at one pane (GOALS: valuable at n=1).
const FIRST_RUN_PRESET: &str = "solo";

pub fn preset_names() -> Vec<&'static str> {
    PRESETS.iter().map(|(n, _)| *n).collect()
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
    let dir = path.parent().expect("workspaces path has a parent");
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let body = toml::to_string_pretty(layout).map_err(|e| e.to_string())?;
    let text = format!("{}{body}", saved_header(name));
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
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

/// The config values these verbs need, read tolerantly.
///
/// The daemon owns config validation (`cyclopsd::Config`), and the CLI
/// cannot depend on the daemon. Same arrangement as the theme key, which
/// `cyclops-theme` reads out of the same file for the same reason: a
/// client reads the keys it needs and stays quiet about the rest.
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
    pub fn read(home: &Path) -> Settings {
        let mut s = Settings {
            sessions: Vec::new(),
            default_workspace: None,
            server: Server::default(),
            exists: false,
        };
        let Ok(text) = std::fs::read_to_string(config_path(home)) else {
            return s;
        };
        s.exists = true;
        let Ok(table) = text.parse::<toml::Table>() else {
            return s;
        };
        if let Some(list) = table.get("sessions").and_then(toml::Value::as_array) {
            s.sessions = list
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect();
        }
        s.default_workspace = table
            .get("default_workspace")
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        s.server.socket = table
            .get("tmux_socket")
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        s.server.config_file = table
            .get("tmux_config")
            .and_then(toml::Value::as_str)
            .map(PathBuf::from);
        s
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

pub fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

/// The size to build a detached session at: this terminal's, when there
/// is one.
///
/// MEASURED on tmux 3.6a (F28): tmux hands the cells from a window resize
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
         # Written by `cyclops start`. Every key: docs/install.md\n\
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
    /// moment ago is normally here: the daemon retries its attach on a
    /// backoff, and until that lands it can resolve none of the session's
    /// panes, so naming one would fail per pane with nothing useful to
    /// say.
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
    let Ok(v) = client.request("status", json!({})) else {
        return Watch::Down;
    };
    let Some(sessions) = v.get("sessions").and_then(Value::as_array) else {
        return Watch::Down;
    };
    let Some(s) = sessions.iter().find(|s| s["name"] == json!(session)) else {
        return Watch::Elsewhere;
    };
    if s["attached"] != json!(true) {
        return Watch::NotYet;
    }
    let mut labels = BTreeMap::new();
    for p in s["panes"].as_array().into_iter().flatten() {
        if let (Some(id), Some(agent)) = (p["pane_id"].as_str(), p["agent"].as_str()) {
            labels.insert(id.to_string(), agent.to_string());
        }
    }
    Watch::Watching(labels)
}

/// Connect without printing. A down daemon is a normal state for these
/// verbs, not an error.
fn daemon() -> Option<Client> {
    match Client::connect() {
        Ok(c) => Some(c),
        Err(ClientError::NotRunning) => None,
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
/// message would go to the wrong agent. GOALS: never type into the wrong
/// pane.
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
/// in proportion (F28), so a workspace built at one terminal size never
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

/// `cyclops start`: have the default workspace open, whatever state the
/// machine was in.
///
/// Steps, in the order they have to happen:
///
/// 1. Resolve which workspace this is, and the layout behind it: the
///    saved file if there is one, else a preset.
/// 2. Build the session if it is not there, and write the workspace file
///    when the layout came from a preset. An existing session is left
///    exactly as it is; this verb is safe to run twice.
/// 3. Make sure the config names the session, so the daemon watches it.
/// 4. Ask the daemon what it is watching and which panes already answer to
///    a name. Step 5 needs both, so it happens first.
/// 5. Decide whether this workspace may name this session's panes at all.
///    Everything after depends on the answer, because a workspace that
///    may not name the session can neither rename its panes nor say how
///    many agents are in it.
/// 6. Put the names back, when the daemon is watching.
/// 7. Say what is ready, and what is left to do.
pub fn run_start(
    json_out: bool,
    style: &Style,
    asked: Option<&str>,
    asked_session: Option<&str>,
    preset_name: Option<&str>,
    launch: bool,
) -> i32 {
    let home = cyclops_proto::cyclops_home();
    let settings = Settings::read(&home);

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
            launch,
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
    }

    // 3.
    if !settings.exists {
        let path = config_path(&home);
        if let Err(e) = write_first_run_config(&home, &session) {
            eprintln!("{e}");
            return 1;
        }
        notes.push(format!("wrote {}", path.display()));
    } else if !settings.watches(&session) {
        notes.push(config_hint(&config_path(&home), &session));
    }

    // 4. The registry is the only record of which pane already answers to
    //    which name, and gate 3 below is that record against the
    //    workspace, so the daemon is asked before anything is decided.
    let mut client = daemon();
    let watch = match client.as_mut() {
        None => Watch::Down,
        Some(c) => watch_of(c, &session),
    };
    let watching = matches!(watch, Watch::Watching(_));
    let live_labels = match &watch {
        Watch::Watching(labels) => labels.clone(),
        _ => BTreeMap::new(),
    };

    // 5. May this workspace put its names on this session's panes? Three
    //    gates, each preventing the same failure from a different
    //    direction, and a no from any of them renames nothing:
    //
    //    1. The workspace has to be this session's. A preset is a shape
    //       cyclops offers, not one the operator chose for a session that
    //       was already open, and adoption is explicit (docs/panes.md).
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

    // 6. Names, and the count the ready line reports.
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
    // already explained itself, and not when the daemon answered: both
    // have said it once already.
    if agents == 0 && layout.agents() > 0 && allowed {
        if let Some(why) = unaskable(&watch, &session) {
            notes.push(names_wait(&name, &session, &why));
        }
    }
    // 7.
    if matches!(watch, Watch::Down) {
        steps.push(("cyclopsd &".into(), "start the daemon".into()));
    }
    if !existed {
        steps.push((
            format!("tmux attach -t {session}"),
            "open the workspace and start your agents".into(),
        ));
    }
    // The last step has to name an agent that will exist: the workspace's
    // first when its names may go on these panes, otherwise one the daemon
    // already holds. A step naming an agent nobody can address is worse
    // than one step fewer.
    let first_name = if allowed {
        layout.panes().find_map(|p| p.label.clone())
    } else {
        live_labels.values().next().cloned()
    };
    if let Some(first) = first_name {
        steps.push((
            format!("cyclops send {first} --subject \"hello\""),
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
                "notes": notes,
            })
        );
        return 0;
    }
    println!("{}", ready_line(agents, watching, style));
    for note in &notes {
        println!("  {}", style.dim(note));
    }
    if !steps.is_empty() {
        println!();
        println!("{}", next_steps(&steps, style));
    }
    0
}

fn write_first_run_config(home: &Path, session: &str) -> Result<(), String> {
    std::fs::create_dir_all(home).map_err(|e| format!("create {}: {e}", home.display()))?;
    let path = config_path(home);
    std::fs::write(&path, first_run_config(session))
        .map_err(|e| format!("write {}: {e}", path.display()))
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
    let settings = Settings::read(&home);
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
    let settings = Settings::read(&home);
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
/// Adoption is explicit (docs/panes.md): cyclops never names a pane
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
    /// names carry over. GOALS makes the ladder law, and four unrelated
    /// arrangements would be four things to learn instead of one.
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

    #[test]
    fn a_saved_workspace_round_trips_through_the_file() {
        let dir = cyclops_proto::scratch::scratch_dir("cyc-ws-file");
        let _ = std::fs::remove_dir_all(&dir);
        let layout = preset("ops").unwrap().unwrap();
        let path = store(&dir, "saved", &layout).expect("store");
        assert!(path.ends_with("workspaces/saved.toml"));
        let back = load(&dir, "saved").expect("file is there").expect("parses");
        assert_eq!(back, layout);
        assert!(load(&dir, "absent").is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
    fn first_run_config_names_the_session_twice() {
        let text = first_run_config("main");
        let table: toml::Table = text.parse().expect("valid toml");
        assert_eq!(table["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(table["sessions"][0].as_str(), Some("main"));
        assert_eq!(table["default_workspace"].as_str(), Some("main"));
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
                "cyclops send implementer --subject \"hello\"".to_string(),
                "send the first message".to_string(),
            ),
        ];
        let expected = "Next:\n\
                        \x20 1  cyclopsd &                                  start the daemon\n\
                        \x20 2  tmux attach -t main                         open the workspace and start your agents\n\
                        \x20 3  cyclops send implementer --subject \"hello\"  send the first message";
        assert_eq!(next_steps(&steps, &plain), expected);
    }
}
