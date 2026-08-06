//! Pane chrome: what an adopted pane says on its own tmux border.
//!
//! An adopted pane's border reads `role • state` in the theme's colors.
//! Nothing here runs on a clock: every write rides an edge that already
//! happened. The border is written on eight edges and no others, and each
//! one is fired by one named function in the daemon:
//!
//! | Edge | Fired by |
//! |---|---|
//! | adoption | `adopt_pane` |
//! | a fused state change | `fusion::recompute_pane` |
//! | a clear | `unadopt_pane` |
//! | a session attach | `reconcile_adoptions` |
//! | a window move | `move_chrome` |
//! | a pane close | `handle_pane_event` |
//! | daemon shutdown | `restore_all_chrome` |
//! | a theme switch | `reload_theme` |
//!
//! The theme switch is M5's, and it is why the count moved from seven:
//! `theme.reload` repaints every adopted pane, because otherwise a switch
//! reaches the borders only at the next fused state change, which on a
//! calm rig is not a time anyone can name.
//!
//! Four of the eight paint a set of panes (adoption, session attach,
//! window move, theme switch) and they all paint through one function,
//! `paint_adoptions`, so "which panes, with what" has one answer. The
//! other four repaint one pane or hand tmux its own options back.
//!
//! ## The on/off switch
//!
//! `chrome = "off"` in the config means cyclops writes no tmux option at
//! all. That rule is HERE and nowhere else: [`apply`], [`repaint`],
//! [`restore`], and [`restore_window`] each take `enabled` and return
//! before their first command when it is false. Callers do not test it, so
//! "does cyclops write a border" is one question with one answer.
//!
//! [`snapshot`] has no switch, because reading is not writing. Recording
//! "there was nothing here" for a pane or window cyclops never looked at
//! would throw the user's own setting away the day they turn chrome on and
//! later take the name off. Every path that will one day restore something
//! reads first, chrome or no chrome.
//!
//! ## Why the border and not the pane title
//!
//! The brief asked for both. The title is not available: every shipped
//! manifest reads `#{pane_title}` as a sensor (claude's spinner is the
//! title tier, priority 1100), a title write from outside pushes a
//! subscription change like any other (F13), and an agent that publishes
//! its own title overwrites cyclops back inside tmux's one-second tick
//! (F23). Writing it would blind detection to paint decoration. Recorded
//! as F26; the border shows `#{pane_title}` by default, so what cyclops
//! writes here REPLACES that view without touching the value underneath.
//!
//! ## Scope, and how a write is taken back
//!
//! Three writes, all reversible, none server-global:
//!
//! 1. `@cyclops_role` and `@cyclops_state`, per pane (`set -p`). The text.
//! 2. `pane-border-format`, per pane (`set -p`, MEASURED settable per pane
//!    on tmux 3.6a). The styling around that text.
//! 3. `pane-border-status`, per window (`set -w`), because a border with
//!    no status carries no text at all and tmux has no pane scope for it
//!    (F27: `set -p` on it writes the window option anyway).
//!
//! The pane's prior `pane-border-format` and the window's prior
//! `pane-border-status` are snapshotted at adoption into the registry, and
//! put back on `--clear`, on pane close, on a window move (the window the
//! pane left), and on daemon shutdown.
//!
//! ## Why the text lives in options instead of in the format
//!
//! tmux expands a format string ONCE, so an option's value is substituted
//! literally and never re-expanded (MEASURED: a value containing
//! `#{pane_id}` renders as those characters). Labels are human input; put
//! one in the format string directly and it becomes a tmux directive
//! evaluated on every border redraw. Splitting them means cyclops owns
//! every `#[...]` in the format and the user owns every character of text,
//! and neither can reach into the other.

use cyclops_proto::{state_words, AgentState};
use cyclops_theme::{tokens, Color, Theme};
use cyclops_tmux::{quote_arg, ControlClient, TmuxError};

use crate::registry::{Adoption, WindowChrome};

/// Per-pane option holding the agent's label.
const OPT_ROLE: &str = "@cyclops_role";
/// Per-pane option holding the state cell's words.
const OPT_STATE: &str = "@cyclops_state";
/// The pane option cyclops rewrites; snapshotted before the first write.
const OPT_FORMAT: &str = "pane-border-format";
/// The window option that decides whether a border carries text.
const OPT_STATUS: &str = "pane-border-status";
/// What cyclops turns the window's border status to.
const STATUS_ON: &str = "top";

/// The prior chrome of one pane and its window, as read before cyclops
/// wrote anything. `None` means the option was unset AT THAT SCOPE, which
/// restore reproduces by unsetting it again.
pub(crate) struct Snapshot {
    pub(crate) border_format: Option<String>,
    pub(crate) border_status: Option<String>,
}

impl Snapshot {
    /// Nothing known, which restores by unsetting. What a failed read
    /// falls back to, and what an adoption already holding both halves on
    /// file starts from.
    pub(crate) fn none() -> Snapshot {
        Snapshot {
            border_format: None,
            border_status: None,
        }
    }
}

/// Read the chrome a pane and its window wear right now. No `enabled`:
/// reading is what keeps a later restore honest, so it happens whether or
/// not cyclops is going to paint (see the module header).
///
/// Two reads per option and not one: `show -v` prints an empty line for
/// "unset here" and for "set here to the empty string" alike, so the
/// value-less form answers whether the option is set at this scope and the
/// `-v` form answers what it is.
pub(crate) async fn snapshot(
    client: &ControlClient,
    pane_id: &str,
    window_id: &str,
) -> Result<Snapshot, TmuxError> {
    Ok(Snapshot {
        border_format: read_scoped(client, "-p", pane_id, OPT_FORMAT).await?,
        border_status: read_scoped(client, "-w", window_id, OPT_STATUS).await?,
    })
}

/// One option's value at one scope, or None when it is not set there.
async fn read_scoped(
    client: &ControlClient,
    scope: &str,
    target: &str,
    option: &str,
) -> Result<Option<String>, TmuxError> {
    let set = client
        .command(&format!(
            "show-options {scope} -t {} {}",
            quote_arg(target),
            quote_arg(option)
        ))
        .await?;
    if set.iter().all(|l| l.trim().is_empty()) {
        return Ok(None);
    }
    let value = client
        .command(&format!(
            "show-options {scope} -t {} -v {}",
            quote_arg(target),
            quote_arg(option)
        ))
        .await?;
    Ok(Some(value.join("\n")))
}

/// Paint one adopted pane's border with `role • state`.
///
/// Idempotent: it writes the same three values for the same inputs, so a
/// repeated call after a reattach costs three commands and changes nothing.
pub(crate) async fn apply(
    client: &ControlClient,
    enabled: bool,
    pane_id: &str,
    window_id: &str,
    label: &str,
    state: AgentState,
    theme: &Theme,
) -> Result<(), TmuxError> {
    if !enabled {
        return Ok(());
    }
    set_scoped(client, "-p", pane_id, OPT_ROLE, label).await?;
    set_scoped(client, "-p", pane_id, OPT_STATE, &state_words(state)).await?;
    set_scoped(
        client,
        "-p",
        pane_id,
        OPT_FORMAT,
        &border_format(theme, label, state),
    )
    .await?;
    set_scoped(client, "-w", window_id, OPT_STATUS, STATUS_ON).await
}

/// Update only what a fused state change moves: the state words and the
/// color around them. The label and the window's border status are already
/// where adoption put them.
pub(crate) async fn repaint(
    client: &ControlClient,
    enabled: bool,
    pane_id: &str,
    label: &str,
    state: AgentState,
    theme: &Theme,
) -> Result<(), TmuxError> {
    if !enabled {
        return Ok(());
    }
    set_scoped(client, "-p", pane_id, OPT_STATE, &state_words(state)).await?;
    set_scoped(
        client,
        "-p",
        pane_id,
        OPT_FORMAT,
        &border_format(theme, label, state),
    )
    .await
}

/// Put a pane's border back the way it was found.
///
/// `window_snapshot` carries the window's prior border status when THIS
/// pane is the one that hands it back: the last adopted pane out on
/// `--clear`, the first one reached at shutdown. None leaves the window's
/// border text on, because another adopted pane is still under it or
/// another pass is already handling it.
pub(crate) async fn restore(
    client: &ControlClient,
    enabled: bool,
    adoption: &Adoption,
    window_snapshot: Option<&WindowChrome>,
) -> Result<(), TmuxError> {
    if !enabled {
        return Ok(());
    }
    unset_scoped(client, "-p", &adoption.pane_id, OPT_ROLE).await?;
    unset_scoped(client, "-p", &adoption.pane_id, OPT_STATE).await?;
    match &adoption.border_format {
        Some(prior) => set_scoped(client, "-p", &adoption.pane_id, OPT_FORMAT, prior).await?,
        None => unset_scoped(client, "-p", &adoption.pane_id, OPT_FORMAT).await?,
    }
    match window_snapshot {
        None => Ok(()),
        Some(w) => {
            restore_window(
                client,
                enabled,
                &adoption.window_id,
                w.border_status.as_deref(),
            )
            .await
        }
    }
}

/// Put a window's border status back. Separate from [`restore`] because a
/// pane that CLOSED takes its own options with it: asking tmux to unset an
/// option on a pane id that no longer exists is an error, and it would
/// abandon the window half of the restore on the way past.
pub(crate) async fn restore_window(
    client: &ControlClient,
    enabled: bool,
    window_id: &str,
    prior: Option<&str>,
) -> Result<(), TmuxError> {
    if !enabled {
        return Ok(());
    }
    match prior {
        Some(p) => set_scoped(client, "-w", window_id, OPT_STATUS, p).await,
        None => unset_scoped(client, "-w", window_id, OPT_STATUS).await,
    }
}

async fn set_scoped(
    client: &ControlClient,
    scope: &str,
    target: &str,
    option: &str,
    value: &str,
) -> Result<(), TmuxError> {
    client
        .command(&format!(
            "set-option {scope} -t {} {} {}",
            quote_arg(target),
            quote_arg(option),
            quote_arg(value)
        ))
        .await
        .map(|_| ())
}

async fn unset_scoped(
    client: &ControlClient,
    scope: &str,
    target: &str,
    option: &str,
) -> Result<(), TmuxError> {
    client
        .command(&format!(
            "set-option {scope} -t {} -u {}",
            quote_arg(target),
            quote_arg(option)
        ))
        .await
        .map(|_| ())
}

/// The format string cyclops installs on an adopted pane.
///
/// Every `#[...]` here is cyclops's; every character the user typed sits
/// behind `#{@cyclops_*}` and cannot become one. The two meaning-carrying
/// encodings stay in separate cells (GOALS): the role hue paints the name,
/// the state group color paints the glyph and the word, and the separator
/// is dim like every other separator in the product.
fn border_format(theme: &Theme, label: &str, state: AgentState) -> String {
    format!(
        " #[fg={}]#{{{OPT_ROLE}}}#[fg={}] • #[fg={}]#{{{OPT_STATE}}}#[default] ",
        style_color(theme.role(label)),
        style_color(theme.resolve(tokens::SURFACE_DIM)),
        style_color(theme.resolve(cyclops_theme::state_token(state))),
    )
}

/// One theme color as a tmux style value.
///
/// Hex rather than the theme's 256-color fallback on purpose. The daemon
/// writes one border for every client that may attach to the session, and
/// those clients can have different terminals; tmux is the only party that
/// knows what each one supports, and it maps a hex color down per client
/// (MEASURED: `#d19a66` renders as SGR 38;5;173 on a 256-color client).
fn style_color(c: Color) -> String {
    let (r, g, b) = c.rgb;
    format!("#{r:02x}{g:02x}{b:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every edge a border write rides, and the function that fires it.
    ///
    /// This is a SET, not a sentence. The old version of this test held
    /// three pages to one identical prose string, so when M5 added the
    /// theme-switch edge and nobody touched the string, all three pages
    /// went on saying "seven" together and the test kept passing on a lie.
    /// Now each edge is checked on its own, and
    /// [`every_border_write_belongs_to_a_named_edge`] reads the daemon's
    /// own source, so a ninth caller fails the build.
    const WRITE_EDGES: [(&str, &str); 8] = [
        ("adoption", "adopt_pane"),
        ("a fused state change", "fusion::recompute_pane"),
        ("a clear", "unadopt_pane"),
        ("a session attach", "reconcile_adoptions"),
        ("a window move", "move_chrome"),
        ("a pane close", "handle_pane_event"),
        ("daemon shutdown", "restore_all_chrome"),
        ("a theme switch", "reload_theme"),
    ];

    /// Functions that carry a write for someone else. Each is reached from
    /// an edge above and decides nothing itself, so it is not an edge.
    const RELAYS: [&str; 4] = [
        "paint_chrome",
        "paint_adoptions",
        "repaint_chrome",
        "restore_for_clear",
    ];

    /// What a chrome write looks like in the source: the four writing
    /// helpers in this module, plus the relays that call them.
    const WRITE_CALLS: [&str; 8] = [
        "chrome::apply(",
        "chrome::repaint(",
        "chrome::restore(",
        "chrome::restore_window(",
        "paint_chrome(",
        "paint_adoptions(",
        "repaint_chrome(",
        "restore_for_clear(",
    ];

    /// The count the three pages state in words, derived so a ninth edge
    /// changes what every page has to say.
    fn count_word(n: usize) -> &'static str {
        match n {
            7 => "seven",
            8 => "eight",
            9 => "nine",
            other => panic!("no word written down for {other} edges"),
        }
    }

    /// The three pages a reader can land on: this file, the page that
    /// documents the border, and the architecture diagram. Each edge and
    /// each firing function is checked separately, so a page can no longer
    /// stay in step with the other two by repeating the same wrong list.
    #[test]
    fn the_three_pages_name_the_same_edges() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for path in [
            root.join("src/chrome.rs"),
            root.join("../../docs/guides/panes.md"),
            root.join("../../docs/development/ARCHITECTURE.md"),
        ] {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            // Only the module header of a Rust file: the arrays above are
            // further down this same file, and matching those would make
            // the test pass itself.
            let prose = if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                text.lines()
                    .filter(|l| l.starts_with("//!"))
                    .map(|l| l.trim_start_matches("//!"))
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                text
            };
            // Line wrapping differs between a doc comment, a markdown
            // table, and a mermaid label, which breaks with <br/> because
            // it has no newline. The words do not differ.
            let flat = prose.replace("<br/>", " ");
            let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
            let total = format!("on {} edges and no others", count_word(WRITE_EDGES.len()));
            assert!(
                flat.contains(&total),
                "{} does not say the border is written \"{total}\"",
                path.display()
            );
            for (edge, fires_from) in WRITE_EDGES {
                assert!(
                    flat.contains(edge),
                    "{} does not name the \"{edge}\" edge",
                    path.display()
                );
                assert!(
                    flat.contains(fires_from),
                    "{} names the \"{edge}\" edge without naming `{fires_from}`, \
                     which is the function that fires it",
                    path.display()
                );
            }
        }
    }

    /// The set above against the daemon's own source: every function that
    /// reaches a border write is either an edge or a relay for one.
    ///
    /// This is the half the prose check could never do. Adding a caller is
    /// how the seventh edge became an eighth silently; now the caller has
    /// to be named here, and naming it here fails the page check above
    /// until all three pages carry it.
    #[test]
    fn every_border_write_belongs_to_a_named_edge() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut unaccounted: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&src).expect("read src").flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // This module defines the write helpers and quotes their names
            // in the arrays above, so scanning it finds its own strings.
            // Nothing here calls them; the callers are all outside.
            if path.ends_with("chrome.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            let mut current = String::new();
            for line in text.lines() {
                let trimmed = line.trim_start();
                // Comments name these functions constantly, including the
                // arrays above. Only code counts as a call site.
                if trimmed.starts_with("//") {
                    continue;
                }
                if let Some(name) = fn_name(trimmed) {
                    current = name;
                }
                if !WRITE_CALLS.iter().any(|c| trimmed.contains(c)) {
                    continue;
                }
                let known = RELAYS.contains(&current.as_str())
                    || WRITE_EDGES
                        .iter()
                        .any(|(_, f)| f.rsplit("::").next() == Some(current.as_str()));
                if !known {
                    unaccounted.push(format!("{}: {current}", path.display()));
                }
            }
        }
        unaccounted.sort();
        unaccounted.dedup();
        assert!(
            unaccounted.is_empty(),
            "these functions write a pane border without being one of the \
             {} edges in WRITE_EDGES or a relay for one, so the three pages \
             documenting the edge set are now wrong: {unaccounted:#?}",
            WRITE_EDGES.len()
        );
    }

    /// The name a line DECLARES, in `fn name(` and in every combination of
    /// visibility and `async` in front of it. None when the line declares
    /// no function, including a line that merely mentions one.
    fn fn_name(line: &str) -> Option<String> {
        let mut rest = line;
        while let Some(word) = ["pub(crate) ", "pub(super) ", "pub ", "async ", "unsafe "]
            .into_iter()
            .find(|w| rest.starts_with(w))
        {
            rest = &rest[word.len()..];
        }
        let name = rest.strip_prefix("fn ")?.split(['(', '<']).next()?.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        Some(name.to_string())
    }

    #[test]
    fn the_format_puts_every_directive_on_cyclops_side() {
        let f = border_format(&Theme::default(), "reviewer", AgentState::Working);
        // The label is referenced, never inlined: a label containing a
        // format directive can then only ever be text.
        assert!(f.contains("#{@cyclops_role}"), "{f}");
        assert!(f.contains("#{@cyclops_state}"), "{f}");
        assert!(!f.contains("reviewer"), "{f}");
        assert!(f.ends_with("#[default] "), "{f}");
    }

    /// The two encodings never share a cell: the name wears the role hue,
    /// the state cell wears its group color, and a state that changes group
    /// changes the format string. Both come from cyclops-theme, so a theme
    /// edit moves the borders with every other surface.
    #[test]
    fn the_name_and_the_state_take_different_colors() {
        let theme = Theme::default();
        let role = style_color(theme.role("reviewer"));
        let working = style_color(theme.resolve(tokens::STATE_HEALTHY));
        let blocked = style_color(theme.resolve(tokens::STATE_NEEDS_YOU));
        assert_ne!(working, blocked);

        let f = border_format(&theme, "reviewer", AgentState::Working);
        assert!(
            f.contains(&format!("#[fg={role}]#{{@cyclops_role}}")),
            "{f}"
        );
        assert!(
            f.contains(&format!("#[fg={working}]#{{@cyclops_state}}")),
            "{f}"
        );

        let b = border_format(&theme, "reviewer", AgentState::BlockedPermission);
        assert!(
            b.contains(&format!("#[fg={blocked}]#{{@cyclops_state}}")),
            "{b}"
        );
    }

    /// A tmux style takes `#rrggbb`; anything else silently renders as
    /// literal text on the border.
    #[test]
    fn colors_are_written_as_tmux_hex() {
        let c = Color {
            rgb: (209, 154, 102),
            c256: 173,
        };
        assert_eq!(style_color(c), "#d19a66");
        assert_eq!(
            style_color(Color {
                rgb: (0, 0, 0),
                c256: 0
            }),
            "#000000"
        );
    }
}
