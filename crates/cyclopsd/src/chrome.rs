//! Pane chrome: what an adopted pane says on its own tmux border.
//!
//! An adopted pane's border reads `role • state` in the theme's colors.
//! Nothing here runs on a clock: every write rides an edge that already
//! happened. The border is written on seven edges and no others: adoption,
//! a fused state change, clear, session attach, a window move, pane close,
//! and daemon shutdown.
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

    /// The edges a border write happens on, word for word. Three pages
    /// state this rule and they are not allowed to drift apart.
    const WRITE_EDGES: &str = "on seven edges and no others: adoption, a fused state change, \
        clear, session attach, a window move, pane close, and daemon shutdown";

    /// The rule lives in three places a reader can land on: this file, the
    /// page that documents the border, and the architecture diagram. An
    /// edge added to or removed from the code has to move all three, or two
    /// of them go on describing the old set.
    #[test]
    fn the_write_edges_are_stated_the_same_way_everywhere() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for path in [
            root.join("src/chrome.rs"),
            root.join("../../docs/panes.md"),
            root.join("../../docs/ARCHITECTURE.md"),
        ] {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            // Only the module header of a Rust file: WRITE_EDGES itself is
            // further down this same file, and matching that would make the
            // test pass itself.
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
            // paragraph, and a mermaid label, which breaks with <br/>
            // because it has no newline. The words do not differ.
            let flat = prose.replace("<br/>", " ");
            let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                flat.contains(WRITE_EDGES),
                "{} does not state the border write rule as \"{WRITE_EDGES}\"",
                path.display()
            );
        }
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
