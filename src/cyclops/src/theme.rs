//! `cyclops theme`: what the themes look like, and which one is on.
//!
//! Two verbs in one command. With no argument it lists the themes it can
//! find and paints each row in its own theme, so a reader picks by looking
//! instead of by reading hex. With a name it writes the `theme` key in
//! `$CYCLOPS_HOME/config.toml` and the choice takes effect everywhere:
//!
//! - one-shot commands read the selection at startup, so the next one
//!   already has it,
//! - `cyclops ui` and the daemon's pane borders hold a
//!   `cyclops_theme::ThemeWatch`, which watches the config key as well as
//!   the theme file, so they pick the switch up on their next repaint,
//!   and the daemon nudge below makes that repaint happen now rather than
//!   on the next thing an agent does.
//!
//! Nothing here decides a color. The swatch resolves the same tokens every
//! other surface resolves, through the same engine, so a preview that
//! looked wrong would mean the theme is wrong.

use std::path::{Path, PathBuf};

use cyclops_proto::{AgentState, EYE_OPEN};
use cyclops_theme::Theme;
use cyclops_ui::grid;
use serde_json::json;

use crate::client::Client;
use crate::copy;
use crate::style::Style;

/// Usage mistakes exit 2, matching the rest of the CLI.
const EXIT_USAGE: i32 = 2;

/// The states the swatch shows, one per state-group token: healthy,
/// needs-you, terminal, quiet, dead.
///
/// The shortest member of each group, because the row has to fit a
/// terminal: the groups are what a theme colors, and which state stands
/// for a group changes nothing about the color. `cyclops_theme::state_token`
/// maps them, so a regrouping moves this row with everything else.
const SWATCH_STATES: [AgentState; 5] = [
    AgentState::Working,
    AgentState::BlockedModal,
    AgentState::BlockedQuota,
    AgentState::Idle,
    AgentState::Dead,
];

/// One theme, as the listing knows it.
struct Entry {
    /// The name to pass back to `cyclops theme <name>`: the file stem, not
    /// the file's own `name` key, because the stem is what selection
    /// resolves and the two can differ.
    name: String,
    path: PathBuf,
    theme: Theme,
    active: bool,
}

/// `cyclops theme [name]`.
pub fn run(json: bool, style: &Style, name: Option<&str>) -> i32 {
    match name {
        Some(name) => set(json, style, name),
        None => list(json, style),
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// Every theme in the themes directory, plus which one is on.
fn list(json: bool, style: &Style) -> i32 {
    let home = cyclops_proto::cyclops_home();
    let active = cyclops_theme::active(&home);
    let entries = entries(&home, active.path.as_deref());

    if json {
        let rows: Vec<_> = entries
            .iter()
            .map(|e| json!({"name": e.name, "path": e.path, "active": e.active}))
            .collect();
        println!("{}", json!({"themes": rows}));
        return 0;
    }

    if entries.is_empty() {
        println!("{}", copy::no_themes(&home));
        return 0;
    }
    let width = entries
        .iter()
        .map(|e| grid::display_width(&e.name))
        .max()
        .unwrap_or(0);
    for e in &entries {
        println!(
            "{}",
            swatch(&e.name, width, e.active, &style.wearing(&e.theme))
        );
    }
    // The active theme is not one of these rows when it was chosen by path
    // or by CYCLOPS_THEME. Saying which file is on beats an unmarked list.
    if !entries.iter().any(|e| e.active) {
        println!();
        println!("  {}", style.dim(&copy::active_elsewhere(&active)));
    }
    println!();
    println!("  {}", style.dim("cyclops theme <name> to switch"));
    0
}

/// The themes directory read into rows, sorted by name, each loaded so it
/// can paint its own swatch.
///
/// Two kinds of file are left out, for one reason: the listing is an offer
/// to switch, and offering a theme that would come up as built-in colors
/// is a lie the reader only finds out later.
///
/// 1. It will not load at all (broken TOML).
/// 2. It loads and sets no token in the vocabulary. Empty, `name` and
///    nothing else, or every token name stale: the loader is deliberately
///    tolerant, so all three parse and all three resolve every token off
///    the compiled default table. The row would preview built-in colors
///    under that file's name and switching to it would repaint nothing.
///
/// [`set`] refuses the same two by name, so a name typed by hand cannot
/// reach what the listing will not offer.
fn entries(home: &Path, active: Option<&Path>) -> Vec<Entry> {
    let Some(dir) = cyclops_theme::themes_dir(home) else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = read
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_stem()?.to_string_lossy().into_owned();
            let (theme, _) = Theme::load(&path).ok()?;
            if !theme.paints_anything() {
                return None;
            }
            let active = active.is_some_and(|a| a == path);
            Some(Entry {
                name,
                path,
                theme,
                active,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// One theme's row, painted in that theme: the name, a cell from every
/// state group, and the eye open.
///
/// `style` must already be wearing the theme this row is for. Both
/// meaning-carrying encodings are not here: role color is per agent and a
/// theme has no agents to color, so the eight slots are pinned pairwise
/// distinct by the engine's tests and seen for real in `cyclops list`.
///
/// The marker is the stream's own marker for "this one" (frame.rs), in the
/// theme's accent, so a reader who has seen the stream already knows it.
fn swatch(name: &str, width: usize, active: bool, style: &Style) -> String {
    let mark = if active {
        format!("{} ", style.accent("▸"))
    } else {
        "  ".to_string()
    };
    let cells: Vec<String> = SWATCH_STATES
        .iter()
        .map(|s| grid::state_cell(*s, style))
        .collect();
    format!(
        "{mark}{}  {}  {}",
        style.dim(&pad(name, width)),
        cells.join("  "),
        style.eye(false, EYE_OPEN)
    )
}

/// Pad to a column width measured in display columns, never bytes.
fn pad(s: &str, width: usize) -> String {
    let w = grid::display_width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

// ---------------------------------------------------------------------------
// Switching
// ---------------------------------------------------------------------------

/// How far the switch got, which is what the line above the swatch has to
/// say. Not three degrees of one thing: the config is written in all
/// three, and what differs is who has confirmed the screen.
enum Switch {
    /// A daemon answered naming the theme just chosen. Pane borders and
    /// any running `cyclops ui` are on it now.
    Live,
    /// Nothing answered on the socket. There is no screen to be wrong
    /// about; the next command reads the key.
    SavedNoDaemon,
    /// A daemon answered and did not take the switch, so the borders on
    /// screen are still some other palette. `Some` carries what it says it
    /// is painting, `None` a daemon that refused the method outright.
    SavedNotLive(Option<String>),
}

/// `cyclops theme <name>`: write the config key, then make it live.
///
/// Steps, in the order that keeps a bad name from costing anything:
///
/// 1. Resolve the name the way selection will resolve it, and load it. A
///    key pointing at a file that does not load leaves every surface on
///    built-in colors and only says so at the next command.
/// 2. Refuse a file that loads and sets nothing, for the same reason and
///    with the same result: it would repaint not one cell (see [`entries`],
///    which leaves it out of the listing for that reason).
/// 3. Write the key, keeping the rest of the file exactly as written.
/// 4. Tell the daemon, so pane borders and any running `cyclops ui`
///    repaint now instead of on the next thing an agent does. Optional by
///    construction: the config is already written, and a down daemon
///    costs the switch nothing but the immediacy.
/// 5. Print what happened, then the swatch of what was chosen, in the
///    theme that was chosen. The glyph on that first line follows step 4's
///    answer and nothing else: see [`nudge_daemon`].
fn set(json: bool, style: &Style, name: &str) -> i32 {
    let home = cyclops_proto::cyclops_home();
    let Some(path) = cyclops_theme::path_for(name, &home) else {
        eprintln!("{}", copy::no_themes(&home));
        return EXIT_USAGE;
    };
    let theme = match Theme::load(&path) {
        Ok((t, warnings)) => {
            for w in warnings {
                eprintln!("theme: {w}");
            }
            t
        }
        Err(e) => {
            eprintln!("{}", copy::theme_unusable(name, &e.to_string()));
            return EXIT_USAGE;
        }
    };
    if !theme.paints_anything() {
        eprintln!("{}", copy::theme_sets_no_colors(name, &path));
        return EXIT_USAGE;
    }

    let config = home.join("config.toml");
    if let Err(e) = cyclops_theme::set_config_theme(&home, name) {
        eprintln!("{}", copy::theme_not_saved(&config, &e));
        return 1;
    }

    // The daemon names the theme it is painting, and a theme's own `name`
    // key is what it names: the file stem the user typed can differ.
    let switch = nudge_daemon(theme.name());

    if json {
        let mut answer = json!({
            "theme": name,
            "path": path,
            "config": config,
            "live": matches!(switch, Switch::Live),
        });
        // Additive: a script that only reads `live` sees what it always
        // saw, and one that wants to know why gets the daemon's answer.
        if let Switch::SavedNotLive(Some(painting)) = &switch {
            answer["daemon_theme"] = json!(painting);
        }
        println!("{answer}");
        return 0;
    }

    let sep = style.dim("·");
    match &switch {
        // Heavy by render::check's rule: cyclopsd owns the borders and it
        // confirmed these are on the chosen theme.
        Switch::Live => println!("{} theme {name}", crate::render::check(true)),
        // Light by the same rule: there was nobody to ask.
        Switch::SavedNoDaemon => println!("{} theme {name}", crate::render::check(false)),
        // Neither weight fits. A check answers for a fact, and this fact
        // was contradicted rather than left unconfirmed, so this takes the
        // vocabulary the wait outcomes use for "it did not happen".
        Switch::SavedNotLive(_) => {
            println!("⚠ theme {name} {sep} {}", style.dim("saved, not live"))
        }
    }
    println!(
        "{}",
        swatch(
            name,
            grid::display_width(name),
            false,
            &style.wearing(&theme)
        )
    );
    match &switch {
        Switch::Live => {}
        Switch::SavedNoDaemon => println!("  {}", style.dim(copy::THEME_NEXT_COMMAND)),
        Switch::SavedNotLive(painting) => println!(
            "  {}",
            style.dim(&copy::theme_not_live(painting.as_deref()))
        ),
    }
    0
}

/// Tell a running daemon the theme key moved, and read its answer.
///
/// The answer is the point. `theme.reload` takes no theme name because the
/// daemon re-resolves the selection itself, and it reports back the theme
/// it is NOW painting, which is not always the one just chosen: a
/// `CYCLOPS_THEME` pinned in its environment beats the config key for the
/// life of that process, and a bare name resolves against `./themes`
/// relative to the daemon's working directory when `$CYCLOPS_HOME/themes`
/// does not exist. Both leave the borders exactly where they were while
/// the request returns without an error, and taking that request's success
/// for a repaint put the heavy check on a switch that never reached the
/// screen.
///
/// Best effort still: a down daemon is the difference between "live now"
/// and "live on the next repaint", never between switching and not.
fn nudge_daemon(want: &str) -> Switch {
    let Ok(mut c) = Client::connect() else {
        return Switch::SavedNoDaemon;
    };
    let Ok(answer) = c.request("theme.reload", json!({})) else {
        // It is up and would not do it, so nothing on screen moved and no
        // later command will move it either.
        return Switch::SavedNotLive(None);
    };
    match answer["theme"].as_str() {
        Some(now) if now == want => Switch::Live,
        Some(now) => Switch::SavedNotLive(Some(now.to_string())),
        // An answer that names no theme confirms nothing about the screen.
        None => Switch::SavedNotLive(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_theme::tokens;

    /// A theme whose every group token is a distinct 256-color entry, so a
    /// golden shows which token painted which cell.
    fn fixture() -> Theme {
        let (theme, warnings) = Theme::parse(
            concat!(
                "name = \"fixture\"\n",
                "[surface]\n",
                "dim = { hex = \"#101010\", c256 = 10 }\n",
                "accent = { hex = \"#202020\", c256 = 20 }\n",
                "[eye]\n",
                "alert = { hex = \"#303030\", c256 = 30 }\n",
                "[state]\n",
                "healthy = { hex = \"#010101\", c256 = 41 }\n",
                "needs_you = { hex = \"#020202\", c256 = 42 }\n",
                "terminal = { hex = \"#030303\", c256 = 43 }\n",
                "quiet = { hex = \"#040404\", c256 = 44 }\n",
                "dead = { hex = \"#050505\", c256 = 45 }\n",
            ),
            "fixture",
        )
        .expect("parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        theme
    }

    fn c(n: u8, text: &str) -> String {
        format!("\x1b[38;5;{n}m{text}\x1b[0m")
    }

    /// The swatch with no color at all: every cell still says a glyph and
    /// a word, which is the whole reason color is allowed to be one of the
    /// two encodings. This is also the row's geometry, and it fits 80
    /// columns with the longest shipped name in the column.
    #[test]
    fn the_swatch_reads_the_same_uncolored() {
        let plain = Style::none();
        let row = swatch("high-contrast", 13, false, &plain);
        assert_eq!(
            row,
            "  high-contrast  ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉"
        );
        assert_eq!(grid::display_width(&row), 79);
        // Short names pad to the column, so the cells line up.
        assert_eq!(
            swatch("dark", 13, false, &plain),
            "  dark           ● working  ⚠ blocked_modal  ⊘ blocked_quota  ○ idle  ✗ dead  ◉"
        );
    }

    /// Every cell in the row is painted by the token that colors it
    /// everywhere else, and the active marker wears the theme's accent.
    /// A swatch that painted its own preview would be the one surface that
    /// could look right while the theme is wrong.
    #[test]
    fn every_swatch_cell_comes_from_its_token() {
        let style = Style::with_theme(fixture(), false);
        assert_eq!(
            swatch("fixture", 7, true, &style),
            format!(
                "{} {}  {}  {}  {}  {}  {}  {}",
                c(20, "▸"),
                c(10, "fixture"),
                c(41, "● working"),
                c(42, "⚠ blocked_modal"),
                c(43, "⊘ blocked_quota"),
                c(44, "○ idle"),
                c(45, "✗ dead"),
                c(30, "◉"),
            )
        );
        // The five cells are the five state-group tokens, no more and no
        // fewer: a group added to the engine has to show up in the preview
        // or a theme's users cannot see what they are choosing.
        let shown: Vec<&str> = SWATCH_STATES
            .iter()
            .map(|s| cyclops_theme::state_token(*s))
            .collect();
        for token in tokens::ALL.iter().filter(|t| t.starts_with("state.")) {
            assert!(shown.contains(token), "{token} is in no swatch cell");
        }
        assert_eq!(
            shown.len(),
            shown
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        );
    }
}
