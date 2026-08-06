//! Paint context for the stream, resolved through the shared theme engine
//! (cyclops-theme). No raw colors live in this crate: every paint names a
//! semantic token, the active theme file decides what it looks like, and
//! the role hash is the engine's, so an agent wears one color across the
//! CLI, receipts, and the stream. Exactly two encodings carry meaning
//! (GOALS): role color and state glyph.
//!
//! Which token a state or a badge takes is `cyclops_theme::state_token` /
//! `delivery_token` and nothing here, and the cells themselves are
//! composed by `crate::grid` through the [`crate::grid::Paint`] impl at
//! the bottom of this file. The CLI's `Style` implements the same trait,
//! so the two surfaces cannot group a state two ways or paint one and
//! leave the other bare.
//!
//! THE EYE, the one deliberate risk (GOALS), has its color half here and
//! nothing else. The glyphs, the positions, and the count they answer to
//! live with the rule in `cyclops_proto::attention`, because three
//! surfaces draw them and only one may decide them. This module paints
//! whatever that composition hands it, with the `eye.calm` / `eye.alert`
//! tokens.
//!
//! The eye ticks through at most one intermediate frame on a state change
//! and never animates continuously (motion restraint). `--plain` renders
//! it as the words closed / opening / open.

use std::io::IsTerminal;

use cyclops_theme::{tokens, Color};

/// Resolved painting context: whether to color at all, how deep, and the
/// active token table.
#[derive(Clone)]
pub struct Theme {
    pub color: bool,
    truecolor: bool,
    engine: cyclops_theme::Theme,
}

impl Theme {
    /// Detect from NO_COLOR, terminal presence, and COLORTERM; load the
    /// active theme only when color is on. Theme warnings surface once on
    /// stderr, before any screen takeover.
    ///
    /// Screen mode is not an input here. `--plain` picks the follow mode,
    /// which builds its own uncolored [`Theme::none`] (plain.rs), so this
    /// only ever answers for the full UI. Taking it as a parameter was the
    /// plain-means-no-color conflation the follow mode already dropped.
    pub fn detect() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let color = !no_color && std::io::stdout().is_terminal();
        let engine = if color {
            let sel = cyclops_theme::active(&cyclops_proto::cyclops_home());
            for w in &sel.warnings {
                eprintln!("theme: {w}");
            }
            sel.theme
        } else {
            cyclops_theme::Theme::default()
        };
        Theme {
            color,
            truecolor: std::env::var("COLORTERM").as_deref() == Ok("truecolor"),
            engine,
        }
    }

    /// No decoration at all. Tests and the plain follow mode use this.
    pub fn none() -> Self {
        Theme {
            color: false,
            truecolor: false,
            engine: cyclops_theme::Theme::default(),
        }
    }

    /// Swap in a reloaded token table (hot reload: the TUI refreshes its
    /// ThemeWatch on events that already woke it, never on a timer).
    pub fn set_engine(&mut self, engine: cyclops_theme::Theme) {
        self.engine = engine;
    }

    /// Color on, with a token table handed in rather than read from disk.
    /// For tests that assert paint: `detect` answers to the environment
    /// and the fields are private, so there is no other way to pin a
    /// color. Mirrors the CLI's `Style::with_theme`.
    #[cfg(test)]
    pub(crate) fn with_engine(engine: cyclops_theme::Theme, truecolor: bool) -> Theme {
        Theme {
            color: true,
            truecolor,
            engine,
        }
    }

    fn paint(&self, c: Color, text: &str) -> String {
        if !self.color {
            return text.to_string();
        }
        if self.truecolor {
            let (r, g, b) = c.rgb;
            format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
        } else {
            format!("\x1b[38;5;{}m{text}\x1b[0m", c.c256)
        }
    }

    /// Paint text in the label's stable role color (the engine's hash, so
    /// the CLI and the stream agree byte for byte).
    pub fn role(&self, label: &str, text: &str) -> String {
        self.paint(self.engine.role(label), text)
    }

    pub fn accent(&self, text: &str) -> String {
        self.paint(self.engine.resolve(tokens::SURFACE_ACCENT), text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint(self.engine.resolve(tokens::SURFACE_DIM), text)
    }

    /// The eye's color tokens: eye.calm closed, eye.alert otherwise.
    pub fn eye(&self, calm: bool, text: &str) -> String {
        let token = if calm {
            tokens::EYE_CALM
        } else {
            tokens::EYE_ALERT
        };
        self.paint(self.engine.resolve(token), text)
    }

    pub fn bold(&self, text: &str) -> String {
        if !self.color {
            return text.to_string();
        }
        format!("\x1b[1m{text}\x1b[0m")
    }
}

/// The stream's half of the shared cell vocabulary.
///
/// Redundant by construction: `Theme::none` and a `NO_COLOR` run paint
/// nothing, and the cells still carry glyph and word (`crate::grid`).
impl crate::grid::Paint for Theme {
    fn dim(&self, text: &str) -> String {
        Theme::dim(self, text)
    }

    fn state(&self, state: cyclops_proto::AgentState, text: &str) -> String {
        self.paint(self.engine.resolve(cyclops_theme::state_token(state)), text)
    }

    fn badge(&self, state: cyclops_proto::DeliveryState, text: &str) -> String {
        self.paint(
            self.engine.resolve(cyclops_theme::delivery_token(state)),
            text,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{EYE_CLOSED, EYE_OPEN, EYE_OPENING};

    fn colored(truecolor: bool) -> Theme {
        Theme {
            color: true,
            truecolor,
            engine: cyclops_theme::Theme::default(),
        }
    }

    #[test]
    fn truecolor_and_256_sequences_from_the_default_table() {
        assert!(colored(true).dim("x").starts_with("\x1b[38;2;128;128;128m"));
        assert!(colored(false).dim("x").starts_with("\x1b[38;5;245m"));
        assert!(colored(true).eye(true, "x").starts_with("\x1b[38;2;"));
    }

    #[test]
    fn none_leaves_text_untouched() {
        let t = Theme::none();
        assert_eq!(t.role("reviewer", "reviewer"), "reviewer");
        assert_eq!(t.dim("x"), "x");
        assert_eq!(t.bold("x"), "x");
        assert_eq!(t.eye(false, EYE_OPEN), "◉");
    }

    #[test]
    fn role_paint_uses_the_engine_hash() {
        // Byte parity with the CLI: the same crate, the same hash, the
        // same slot table. A drift here re-colors agents between surfaces.
        let t = colored(true);
        let slot = cyclops_theme::role_slot("reviewer");
        let c = cyclops_theme::Theme::default().resolve(cyclops_theme::tokens::ROLE[slot]);
        let (r, g, b) = c.rgb;
        assert!(t
            .role("reviewer", "reviewer")
            .starts_with(&format!("\x1b[38;2;{r};{g};{b}m")));
    }

    #[test]
    fn a_loaded_theme_changes_the_paint() {
        let (engine, warnings) =
            cyclops_theme::Theme::parse("[surface]\ndim = { hex = \"#102030\", c256 = 17 }\n", "t")
                .expect("parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut t = colored(true);
        t.set_engine(engine);
        assert!(t.dim("x").starts_with("\x1b[38;2;16;32;48m"));
    }

    #[test]
    fn eye_glyphs_are_single_column() {
        for g in [EYE_CLOSED, EYE_OPENING, EYE_OPEN] {
            assert_eq!(crate::grid::display_width(g), 1, "{g}");
        }
    }
}
