//! Workspace layouts: the declarative tree, read off a live session and
//! applied to a new one.
//!
//! The tree is a grid of rows, not tmux's binary split tree. A window is
//! rows top to bottom; a row is panes left to right; every size is a ratio
//! rather than a cell count. That covers every shipped preset and every
//! arrangement `select-layout` produces, it reads as data a human can
//! edit, and it survives a resize because nothing in it is a cell count.
//! A window that is not a grid of rows is refused by [`capture`] instead
//! of saved as an approximation that would restore into something else.
//!
//! Ratios are relative and normalized by their own row or window, so
//! `34/33/33` and `0.34/0.33/0.33` mean the same thing.
//!
//! Every ratio here is a share of the cells tmux gave the PANES, never of
//! the window. The difference is what the borders take, and how much they
//! take is not a constant: tmux spends a line between stacked panes and a
//! column between side-by-side ones, and `pane-border-status top`, which
//! cyclops's own pane chrome turns on, costs each pane another line (F27).
//! Measuring against the panes makes the arithmetic the same with chrome
//! on, with chrome off, and under whatever a user's tmux config does.
//!
//! Every tmux invocation here is a one-shot ([`crate::cmd`]). Building a
//! workspace is a user gesture like a focus jump, not daemon state, and it
//! has to work against a server the daemon has never attached to.
//!
//! What this module does NOT do, deliberately:
//!
//! - It sets no tmux options. No `set-option`, no `set-window-option`, no
//!   border formats. Nothing it does needs undoing.
//! - It never writes a pane title. `pane_title` is the title sensor's
//!   input (F5/F6), and a label written there would be read back as agent
//!   state. Labels go to the daemon registry through `pane.label`.
//! - It never restructures a session that already exists. [`apply`]
//!   refuses one, so a workspace file can never eat a running session.
//! - It sends no keys to any pane. A recorded command runs as the pane's
//!   own command or not at all.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cmd::{run, session_target};
use crate::error::TmuxError;

/// Window fields, free text last: a tab inside a window name would
/// otherwise shift every field after it. The window's own size is not
/// among them on purpose: everything here measures panes.
const WINDOW_FORMAT: &str = "#{window_id}\t#{window_zoomed_flag}\t#{window_name}";

/// Pane fields. The two free-text fields trail: `pane_current_command` is
/// a process name and cannot hold a tab, so only a tab inside a path can
/// confuse this, and that path would be pathological.
const PANE_FORMAT: &str = "#{pane_id}\t#{window_id}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}\t#{pane_current_command}\t#{pane_current_path}";

/// Foreground commands [`capture`] does not record as a launch hint. A
/// pane sitting at a shell prompt is a pane with nothing to launch, and
/// recording the shell would make `--launch` mean "start a shell", which
/// is what tmux does anyway.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "fish", "dash", "ksh", "csh", "tcsh"];

/// Which tmux server to address.
///
/// The daemon's config carries the same two values (`tmux_socket`,
/// `tmux_config`), so a client that builds a workspace lands on the server
/// the daemon is watching.
#[derive(Debug, Clone, Default)]
pub struct Server {
    /// `tmux -L` socket name. None uses the server the environment points
    /// at, which inside tmux is the user's own.
    pub socket: Option<String>,
    /// `tmux -f` config file. None uses the user's tmux config.
    pub config_file: Option<PathBuf>,
}

impl Server {
    fn socket(&self) -> Option<&str> {
        self.socket.as_deref()
    }

    fn config(&self) -> Option<&Path> {
        self.config_file.as_deref()
    }

    fn run(&self, args: &[&str]) -> Result<String, TmuxError> {
        run(self.socket(), self.config(), args)
    }
}

/// One workspace layout: what a preset file ships and what
/// `cyclops workspace save` writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// Workspace name, which is also the default session name.
    pub name: String,
    /// One line saying what this arrangement is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    pub windows: Vec<Window>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub name: String,
    /// Top to bottom.
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// Share of the window's pane rows. Relative to the other rows.
    pub ratio: f64,
    /// Left to right.
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    /// Cyclops label this pane is adopted under, e.g. "reviewer". A pane
    /// with no label is not an agent: the `ops` dock holds the stream, and
    /// nothing addresses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Share of its row's pane columns. Relative to the other panes.
    pub ratio: f64,
    /// Working directory. Absent inherits the directory the workspace was
    /// built from, which is what a shipped preset wants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Launch hint: what to run in the pane, and only with
    /// [`ApplyOptions::launch`]. Captured from the foreground command tmux
    /// reported, which carries no arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

impl Layout {
    /// Panes in layout order: windows in order, rows top to bottom, panes
    /// left to right. The order [`capture`] and [`apply`] report pane ids
    /// in, so a caller can zip the two.
    pub fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.windows
            .iter()
            .flat_map(|w| w.rows.iter().flat_map(|r| r.panes.iter()))
    }

    /// How many panes this workspace names as agents. The number
    /// `cyclops start` reports.
    pub fn agents(&self) -> usize {
        self.panes().filter(|p| p.label.is_some()).count()
    }

    /// Total panes.
    pub fn pane_count(&self) -> usize {
        self.panes().count()
    }

    /// Reject a layout that cannot be applied, naming what is wrong.
    ///
    /// Three things make one unapplicable, and each is a real file a user
    /// or a preset could hold: an empty level (nothing to build), a
    /// non-positive ratio (no share of anything), and a label used twice
    /// (the registry holds one pane per label, so the second adoption
    /// would silently move the first).
    pub fn validate(&self) -> Result<(), String> {
        if self.windows.is_empty() {
            return Err(format!("layout \"{}\" has no windows", self.name));
        }
        for w in &self.windows {
            if w.rows.is_empty() {
                return Err(format!("window \"{}\" has no rows", w.name));
            }
            for row in &w.rows {
                if row.panes.is_empty() {
                    return Err(format!("a row of window \"{}\" has no panes", w.name));
                }
                if !usable(row.ratio) {
                    return Err(format!(
                        "a row of window \"{}\" has ratio {}; ratios must be above zero",
                        w.name, row.ratio
                    ));
                }
                for p in &row.panes {
                    if !usable(p.ratio) {
                        return Err(format!(
                            "a pane of window \"{}\" has ratio {}; ratios must be above zero",
                            w.name, p.ratio
                        ));
                    }
                }
            }
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for p in self.panes() {
            if let Some(l) = &p.label {
                if !seen.insert(l.as_str()) {
                    return Err(format!("label \"{l}\" is used by two panes"));
                }
            }
        }
        Ok(())
    }
}

/// A layout read off a live session, with the pane ids it came from.
#[derive(Debug, Clone)]
pub struct Capture {
    pub layout: Layout,
    /// Pane ids in [`Layout::panes`] order.
    pub pane_ids: Vec<String>,
}

/// How [`apply`] builds a layout.
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Run each pane's recorded command instead of leaving a shell. Off by
    /// default: a workspace restores structure, not live processes.
    pub launch: bool,
    /// Size for the detached session tmux creates. None lets tmux use its
    /// own `default-size`, and tmux rescales the ratios when a client
    /// attaches at a different size.
    pub size: Option<(u32, u32)>,
}

/// Does this session exist on this server?
pub fn session_exists(server: &Server, session: &str) -> Result<bool, TmuxError> {
    match server.run(&["has-session", "-t", &session_target(session)]) {
        Ok(_) => Ok(true),
        // tmux says "can't find session" on stderr and exits non-zero.
        // A server that is not running says "no server running"; both mean
        // the same thing to a caller asking whether it can attach.
        Err(TmuxError::Command(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Read a session's structure as a layout.
///
/// Steps, in the order they have to happen:
///
/// 1. List the windows, so every pane can be measured against the window
///    it lives in.
/// 2. List the panes and group them by window.
/// 3. Fold each window's panes into rows by their top edge, refusing a
///    window that is not a grid of rows.
/// 4. Turn cell counts into ratios.
///
/// Labels are not here. Adoption is the daemon's registry, not tmux's, so
/// the caller fills [`Pane::label`] from `status` using [`Capture::pane_ids`].
pub fn capture(server: &Server, session: &str) -> Result<Capture, TmuxError> {
    let target = session_target(session);

    // 1. Windows, in index order (tmux lists them that way).
    let win_text = server.run(&["list-windows", "-t", &target, "-F", WINDOW_FORMAT])?;
    let mut windows: Vec<CapturedWindow> = Vec::new();
    for line in win_text.lines().filter(|l| !l.is_empty()) {
        windows.push(CapturedWindow::parse(line)?);
    }
    if windows.is_empty() {
        return Err(TmuxError::Layout(format!(
            "session \"{session}\" has no windows"
        )));
    }

    // 2. Panes, grouped by window.
    let pane_text = server.run(&["list-panes", "-s", "-t", &target, "-F", PANE_FORMAT])?;
    let mut panes: Vec<CapturedPane> = Vec::new();
    for line in pane_text.lines().filter(|l| !l.is_empty()) {
        panes.push(CapturedPane::parse(line)?);
    }

    let mut out_windows = Vec::with_capacity(windows.len());
    let mut pane_ids = Vec::with_capacity(panes.len());
    for w in &windows {
        // A zoomed pane reports the whole window as its size while its
        // neighbours keep theirs, so the grid check below would call a
        // perfectly ordinary window broken. Name the real problem instead.
        if w.zoomed {
            return Err(TmuxError::Layout(format!(
                "window \"{}\" has a zoomed pane. Unzoom it (prefix then z) and try again.",
                w.name
            )));
        }
        let mut mine: Vec<&CapturedPane> = panes.iter().filter(|p| p.window_id == w.id).collect();
        if mine.is_empty() {
            return Err(TmuxError::Layout(format!(
                "window \"{}\" reported no panes",
                w.name
            )));
        }
        // 3. Rows top to bottom, panes left to right.
        mine.sort_by_key(|p| (p.top, p.left));
        let rows = rows_of(&mine, w)?;
        // 4. Ratios.
        let row_cells: u32 = rows.iter().map(|r| r[0].height).sum();
        let mut out_rows = Vec::with_capacity(rows.len());
        for row in &rows {
            let col_cells: u32 = row.iter().map(|p| p.width).sum();
            let mut out_panes = Vec::with_capacity(row.len());
            for p in row {
                pane_ids.push(p.id.clone());
                out_panes.push(Pane {
                    label: None,
                    ratio: ratio(p.width, col_cells),
                    cwd: Some(p.cwd.clone()).filter(|c| !c.is_empty()),
                    command: launch_hint(&p.command),
                });
            }
            out_rows.push(Row {
                ratio: ratio(row[0].height, row_cells),
                panes: out_panes,
            });
        }
        out_windows.push(Window {
            name: w.name.clone(),
            rows: out_rows,
        });
    }

    Ok(Capture {
        layout: Layout {
            name: session.to_string(),
            about: None,
            windows: out_windows,
        },
        pane_ids,
    })
}

/// Build a layout as a new session, returning the pane ids in
/// [`Layout::panes`] order.
///
/// Steps, in the order they have to happen:
///
/// 1. Refuse an existing session. Applying into one would restructure
///    panes somebody is working in, and a restore is never allowed to do
///    that.
/// 2. Create the session, which is the first window's first pane.
/// 3. Create the remaining windows.
/// 4. Per window: split the rows out of the first pane, top to bottom.
/// 5. Per window: set the row heights, top down. The last row takes what
///    is left, which is why it is not resized.
/// 6. Per window and row: split the panes out, left to right, then set
///    their widths the same way.
///
/// Rows before panes matters: a row is a full-width band, and splitting a
/// pane that is already half a row would nest a pane under it rather than
/// add a band.
pub fn apply(
    server: &Server,
    session: &str,
    layout: &Layout,
    opts: &ApplyOptions,
) -> Result<Vec<String>, TmuxError> {
    layout.validate().map_err(TmuxError::Layout)?;
    // 1.
    if session_exists(server, session)? {
        return Err(TmuxError::Layout(format!(
            "session \"{session}\" already exists. Cyclops does not rearrange a session you are already in."
        )));
    }

    let mut pane_ids = Vec::with_capacity(layout.pane_count());
    for (idx, window) in layout.windows.iter().enumerate() {
        let first = &window.rows[0].panes[0];
        // 2 and 3: the only difference between the first window and the
        // rest is which command makes it.
        let root = if idx == 0 {
            let mut args: Vec<String> = vec![
                "new-session".into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-s".into(),
                session.into(),
                "-n".into(),
                window.name.clone(),
            ];
            if let Some((w, h)) = opts.size {
                args.extend([
                    "-x".to_string(),
                    w.to_string(),
                    "-y".to_string(),
                    h.to_string(),
                ]);
            }
            push_pane_args(&mut args, first, opts);
            server.run(&str_args(&args))?
        } else {
            let mut args: Vec<String> = vec![
                "new-window".into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-t".into(),
                session_target(session),
                "-n".into(),
                window.name.clone(),
            ];
            push_pane_args(&mut args, first, opts);
            server.run(&str_args(&args))?
        };
        let root = root.trim().to_string();

        // 4. Rows, each split off the row above it.
        let mut row_heads = vec![root];
        for row in &window.rows[1..] {
            let anchor = row_heads.last().expect("a row head exists").clone();
            row_heads.push(split(server, "-v", &anchor, &row.panes[0], opts)?);
        }

        // 5. Row heights, shared out over the lines the rows HOLD right
        //    now rather than over the window's. The window also pays for
        //    borders, and how much it pays is not fixed (F27), so asking
        //    the panes is the only arithmetic that stays right whatever
        //    chrome is on. A window too small for the ratios keeps the
        //    even split tmux already gave it: see [`share`].
        let sizes = pane_sizes(server, session)?;
        let ratios: Vec<f64> = window.rows.iter().map(|r| r.ratio).collect();
        let lines = cells(&sizes, &row_heads, |(_, h)| h);
        if let Some(heights) = share(&ratios, lines) {
            for (head, h) in row_heads.iter().zip(&heights).take(window.rows.len() - 1) {
                server.run(&["resize-pane", "-t", head, "-y", &h.to_string()])?;
            }
        }

        // 6. Panes within each row, then their widths the same way. The
        //    sizes are re-read per row because each split changes them.
        for (row, head) in window.rows.iter().zip(&row_heads) {
            let mut ids = vec![head.clone()];
            for pane in &row.panes[1..] {
                let anchor = ids.last().expect("a pane exists").clone();
                ids.push(split(server, "-h", &anchor, pane, opts)?);
            }
            let sizes = pane_sizes(server, session)?;
            let ratios: Vec<f64> = row.panes.iter().map(|p| p.ratio).collect();
            let columns = cells(&sizes, &ids, |(w, _)| w);
            if let Some(widths) = share(&ratios, columns) {
                for (id, w) in ids.iter().zip(&widths).take(row.panes.len() - 1) {
                    server.run(&["resize-pane", "-t", id, "-x", &w.to_string()])?;
                }
            }
            pane_ids.extend(ids);
        }
    }
    Ok(pane_ids)
}

/// Split `anchor` and return the new pane's id. `direction` is `-h` for a
/// pane beside it, `-v` for a pane below it.
fn split(
    server: &Server,
    direction: &str,
    anchor: &str,
    pane: &Pane,
    opts: &ApplyOptions,
) -> Result<String, TmuxError> {
    let mut args: Vec<String> = vec![
        "split-window".into(),
        direction.into(),
        "-d".into(),
        "-P".into(),
        "-F".into(),
        "#{pane_id}".into(),
        "-t".into(),
        anchor.into(),
    ];
    push_pane_args(&mut args, pane, opts);
    Ok(server.run(&str_args(&args))?.trim().to_string())
}

/// The two per-pane arguments every creating command takes: where it
/// starts, and what runs in it.
///
/// The command goes last because tmux reads everything after the options
/// as the shell command. It is only ever added under `--launch`: a file on
/// disk naming a command is a suggestion, and running it has to be the
/// operator's decision each time.
fn push_pane_args(args: &mut Vec<String>, pane: &Pane, opts: &ApplyOptions) {
    if let Some(cwd) = &pane.cwd {
        args.extend(["-c".to_string(), cwd.clone()]);
    }
    if opts.launch {
        if let Some(cmd) = &pane.command {
            args.push(cmd.clone());
        }
    }
}

fn str_args(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

/// Every pane's (width, height) in a session, keyed by pane id.
fn pane_sizes(
    server: &Server,
    session: &str,
) -> Result<std::collections::HashMap<String, (u32, u32)>, TmuxError> {
    let text = server.run(&[
        "list-panes",
        "-s",
        "-t",
        &session_target(session),
        "-F",
        "#{pane_id}\t#{pane_width}\t#{pane_height}",
    ])?;
    let mut out = std::collections::HashMap::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 3 {
            return Err(TmuxError::Layout(format!(
                "tmux pane size line has too few fields: {line:?}"
            )));
        }
        out.insert(f[0].to_string(), (parse_u32(f[1])?, parse_u32(f[2])?));
    }
    Ok(out)
}

/// Cells these panes hold along one axis. A pane tmux no longer reports
/// contributes nothing, which makes the total too small and sends
/// [`share`] down its "leave tmux's split alone" path rather than sizing
/// something against a number that is missing a pane.
fn cells(
    sizes: &std::collections::HashMap<String, (u32, u32)>,
    ids: &[String],
    axis: fn((u32, u32)) -> u32,
) -> u32 {
    ids.iter()
        .map(|id| sizes.get(id).copied().map(axis).unwrap_or(0))
        .sum()
}

/// Cell counts for `ratios` sharing `total` cells, largest remainder first
/// so they add back up to `total`.
///
/// None when the ratios cannot be honored: every share has to be at least
/// one cell, and a window too small for that keeps the even split tmux
/// already gave it. Sizing a pane to zero is the failure this prevents.
fn share(ratios: &[f64], total: u32) -> Option<Vec<u32>> {
    let n = ratios.len() as u32;
    if total < n || n == 0 {
        return None;
    }
    let sum: f64 = ratios.iter().sum();
    if !usable(sum) {
        return None;
    }
    let exact: Vec<f64> = ratios.iter().map(|r| r / sum * total as f64).collect();
    let mut cells: Vec<u32> = exact.iter().map(|e| (e.floor() as u32).max(1)).collect();
    let mut spent: u32 = cells.iter().sum();
    if spent > total {
        return None;
    }
    // Hand out the remainder to the largest fractional parts first.
    let mut order: Vec<usize> = (0..cells.len()).collect();
    order.sort_by(|a, b| {
        let fa = exact[*a] - exact[*a].floor();
        let fb = exact[*b] - exact[*b].floor();
        fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut i = 0;
    while spent < total {
        cells[order[i % order.len()]] += 1;
        spent += 1;
        i += 1;
    }
    Some(cells)
}

/// A ratio that can be shared out. TOML holds `nan` and `inf` as happily
/// as it holds `0.34`, and neither can be turned into cells.
fn usable(ratio: f64) -> bool {
    ratio.is_finite() && ratio > 0.0
}

/// Cell count as a share of a total, to four places so a saved file reads
/// like a decision rather than a float dump.
fn ratio(cells: u32, total: u32) -> f64 {
    if total == 0 {
        return 0.0;
    }
    ((cells as f64 / total as f64) * 10_000.0).round() / 10_000.0
}

/// The launch hint for a foreground command, or None when the pane is
/// sitting at a shell.
fn launch_hint(command: &str) -> Option<String> {
    let c = command.trim();
    if c.is_empty() || SHELLS.contains(&c) {
        None
    } else {
        Some(c.to_string())
    }
}

/// Fold position-sorted panes into full-width rows.
///
/// A row is every pane sharing a top edge, and it is a real row only when
/// the panes in it are one band across the window. Two tests say that, and
/// neither one asks the window how big it is, because the answer changes
/// with the borders (F27) and the arithmetic must not:
///
/// 1. One height per row. A pane beside a taller one is the left half of a
///    nested split, not a member of a row.
/// 2. Every row reaches from the same left edge to the same right edge. A
///    row that stops short is sitting beside something rather than under
///    it.
///
/// What fails both is a layout this grid cannot say. Saving it would
/// restore into a different arrangement, so it is refused by name.
fn rows_of<'a>(
    panes: &[&'a CapturedPane],
    w: &CapturedWindow,
) -> Result<Vec<Vec<&'a CapturedPane>>, TmuxError> {
    let mut rows: Vec<Vec<&CapturedPane>> = Vec::new();
    for p in panes {
        match rows.last_mut() {
            Some(row) if row[0].top == p.top => row.push(p),
            _ => rows.push(vec![p]),
        }
    }
    // Context-neutral on purpose: this sentence is shown by `save`, which
    // is about to write a file, and by `start`, which is about to put
    // names back. Naming either one here would be wrong half the time.
    let not_a_grid = |detail: &str| {
        TmuxError::Layout(format!(
            "window \"{}\" is not a grid of rows ({detail}), so cyclops can't describe it as rows and columns. Even it out with tmux's select-layout (tiled, even-horizontal, even-vertical).",
            w.name
        ))
    };
    let extent = |row: &Vec<&CapturedPane>| -> (u32, u32) {
        let last = row.last().expect("a row has a pane");
        (row[0].left, last.left + last.width)
    };
    let want = extent(&rows[0]);
    for row in &rows {
        if row.iter().any(|p| p.height != row[0].height) {
            return Err(not_a_grid(&format!(
                "the panes starting at line {} are not the same height",
                row[0].top
            )));
        }
        let got = extent(row);
        if got != want {
            return Err(not_a_grid(&format!(
                "the panes starting at line {} run from column {} to {}, and the top row runs {} to {}",
                row[0].top, got.0, got.1, want.0, want.1
            )));
        }
    }
    Ok(rows)
}

/// One window as tmux reported it.
struct CapturedWindow {
    id: String,
    zoomed: bool,
    name: String,
}

impl CapturedWindow {
    fn parse(line: &str) -> Result<CapturedWindow, TmuxError> {
        let mut f = line.splitn(3, '\t');
        match (f.next(), f.next(), f.next()) {
            (Some(id), Some(z), Some(name)) => Ok(CapturedWindow {
                id: id.to_string(),
                zoomed: z == "1",
                name: name.to_string(),
            }),
            _ => Err(TmuxError::Layout(format!(
                "tmux window line has too few fields: {line:?}"
            ))),
        }
    }
}

/// One pane as tmux reported it.
struct CapturedPane {
    id: String,
    window_id: String,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    command: String,
    cwd: String,
}

impl CapturedPane {
    fn parse(line: &str) -> Result<CapturedPane, TmuxError> {
        let f: Vec<&str> = line.splitn(8, '\t').collect();
        if f.len() < 8 {
            return Err(TmuxError::Layout(format!(
                "tmux pane line has too few fields: {line:?}"
            )));
        }
        Ok(CapturedPane {
            id: f[0].to_string(),
            window_id: f[1].to_string(),
            left: parse_u32(f[2])?,
            top: parse_u32(f[3])?,
            width: parse_u32(f[4])?,
            height: parse_u32(f[5])?,
            command: f[6].to_string(),
            cwd: f[7].to_string(),
        })
    }
}

fn parse_u32(s: &str) -> Result<u32, TmuxError> {
    s.trim()
        .parse()
        .map_err(|_| TmuxError::Layout(format!("tmux reported {s:?} where a number belongs")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(label: Option<&str>, ratio: f64) -> Pane {
        Pane {
            label: label.map(str::to_string),
            ratio,
            cwd: None,
            command: None,
        }
    }

    fn one_window(rows: Vec<Row>) -> Layout {
        Layout {
            name: "t".into(),
            about: None,
            windows: vec![Window {
                name: "t".into(),
                rows,
            }],
        }
    }

    #[test]
    fn shares_add_up_to_the_total() {
        // 198 columns at 34/33/33 is 67.32/65.34/65.34: the cell left over
        // after flooring goes to the largest fraction, not to the first.
        assert_eq!(share(&[0.34, 0.33, 0.33], 198), Some(vec![67, 66, 65]));
        assert_eq!(share(&[0.7, 0.3], 49), Some(vec![34, 15]));
        // Relative ratios are the same decision written differently.
        assert_eq!(
            share(&[34.0, 33.0, 33.0], 198),
            share(&[0.34, 0.33, 0.33], 198)
        );
        // Every share adds back to the total, whatever the rounding.
        for total in 3..200u32 {
            let cells = share(&[0.5, 0.3, 0.2], total).expect("fits");
            assert_eq!(cells.iter().sum::<u32>(), total, "total {total}");
            assert!(cells.iter().all(|c| *c >= 1), "total {total}");
        }
    }

    #[test]
    fn a_window_too_small_for_the_ratios_keeps_tmuxs_split() {
        assert_eq!(share(&[0.5, 0.5], 1), None);
        assert_eq!(share(&[0.98, 0.01, 0.01], 2), None);
        assert_eq!(share(&[], 10), None);
        assert_eq!(share(&[f64::NAN, 1.0], 10), None);
    }

    #[test]
    fn validate_names_what_is_wrong() {
        assert!(one_window(vec![Row {
            ratio: 1.0,
            panes: vec![pane(Some("a"), 1.0)]
        }])
        .validate()
        .is_ok());

        let empty = Layout {
            name: "t".into(),
            about: None,
            windows: vec![],
        };
        assert!(empty.validate().unwrap_err().contains("no windows"));

        let zero = one_window(vec![Row {
            ratio: 0.0,
            panes: vec![pane(None, 1.0)],
        }]);
        assert!(zero.validate().unwrap_err().contains("above zero"));

        let twice = one_window(vec![Row {
            ratio: 1.0,
            panes: vec![pane(Some("a"), 1.0), pane(Some("a"), 1.0)],
        }]);
        assert!(twice
            .validate()
            .unwrap_err()
            .contains("\"a\" is used by two panes"));
    }

    #[test]
    fn agents_are_the_labelled_panes() {
        let l = one_window(vec![
            Row {
                ratio: 0.7,
                panes: vec![pane(Some("a"), 1.0), pane(Some("b"), 1.0)],
            },
            Row {
                ratio: 0.3,
                panes: vec![pane(None, 1.0)],
            },
        ]);
        assert_eq!(l.agents(), 2);
        assert_eq!(l.pane_count(), 3);
    }

    #[test]
    fn shells_are_not_launch_hints() {
        assert_eq!(launch_hint("zsh"), None);
        assert_eq!(launch_hint(""), None);
        assert_eq!(launch_hint("claude"), Some("claude".to_string()));
        // F21: a native claude install reports its version as the command.
        assert_eq!(launch_hint("2.1.220"), Some("2.1.220".to_string()));
    }

    #[test]
    fn ratios_round_to_four_places() {
        assert_eq!(ratio(1, 3), 0.3333);
        assert_eq!(ratio(34, 49), 0.6939);
        assert_eq!(ratio(5, 0), 0.0);
    }
}
