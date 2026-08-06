//! Color and weight, resolved through the theme engine (cyclops-theme).
//! No raw color values live in this crate anymore: every paint goes
//! through a semantic token, and the active theme file decides what the
//! token looks like.
//!
//! GOALS.md allows exactly two meaning-carrying encodings: role color and
//! the state glyph. Both have a color half here, and they never share a
//! cell. Role hue paints the agent NAME; a state's group color paints the
//! state cell and the delivery badge, on top of the glyph and word that
//! already said the same thing. Nothing here invents a group: the mapping
//! is cyclops-theme's, so the CLI and the stream cannot group states two
//! ways.
//!
//! The cells themselves are composed by `cyclops_ui::grid` through the
//! [`cyclops_ui::grid::Paint`] impl at the bottom of this file, which the
//! stream's `Theme` also implements. Composing them at the call site is
//! how the CLI came to paint a state cell that the stream rendered bare
//! against the same theme.

use std::io::IsTerminal;

use cyclops_proto::{AgentState, DeliveryState};
use cyclops_theme::{tokens, Color, Theme};

#[derive(Clone)]
pub struct Style {
    /// ANSI output enabled at all.
    pub color: bool,
    truecolor: bool,
    theme: Theme,
}

impl Style {
    /// Detect from the flag, NO_COLOR, terminal presence, and COLORTERM.
    /// NO_COLOR counts when present and non-empty, per its spec. The theme
    /// loads only when color is on: piped and machine output never reads
    /// the filesystem for looks.
    ///
    /// This is the one place the CLI reads the theme file, and it prints
    /// that file's warnings (an explicitly chosen theme that is missing,
    /// unknown tokens) to stderr. So only a command that is about to
    /// render calls it: `cyclops hook` must stay byte-silent, and
    /// `cyclops ui` loads its own theme, so a Style built for either one
    /// broke a contract rather than helping.
    pub fn detect(plain: bool) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let color = !plain && !no_color && std::io::stdout().is_terminal();
        let theme = if color {
            let sel = cyclops_theme::active(&cyclops_proto::cyclops_home());
            for w in &sel.warnings {
                eprintln!("theme: {w}");
            }
            sel.theme
        } else {
            Theme::default()
        };
        Style {
            color,
            truecolor: std::env::var("COLORTERM").as_deref() == Ok("truecolor"),
            theme,
        }
    }

    /// No decoration at all. Tests and machine paths use this.
    pub fn none() -> Self {
        Style {
            color: false,
            truecolor: false,
            theme: Theme::default(),
        }
    }

    /// Color on, with a theme handed in rather than read from disk. For
    /// tests that assert paint: `detect` answers to the environment, and
    /// the fields are private, so there is no other way to pin a color.
    #[cfg(test)]
    pub(crate) fn with_theme(theme: Theme, truecolor: bool) -> Style {
        Style {
            color: true,
            truecolor,
            theme,
        }
    }

    /// This terminal's answer, wearing a different token table.
    ///
    /// `cyclops theme` paints each theme's swatch in that theme while the
    /// run still honors NO_COLOR, `--plain` and what the terminal can
    /// actually emit. Those are the terminal's facts, not the theme's, so
    /// they survive the swap.
    pub fn wearing(&self, theme: &Theme) -> Style {
        Style {
            color: self.color,
            truecolor: self.truecolor,
            theme: theme.clone(),
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

    /// Paint text in the label's stable role color.
    ///
    /// The hash and the slot table are cyclops-theme's (FNV-1a, not
    /// DefaultHasher: a label must keep its color across runs, platforms
    /// and rust versions), so an agent wears one color on every surface.
    pub fn role(&self, label: &str, text: &str) -> String {
        self.paint(self.theme.role(label), text)
    }

    /// The eye: eye.calm when nothing needs the human, eye.alert
    /// otherwise. These are the tokens `cyclops ui` paints its own eye
    /// with, so the mark reads the same on both surfaces.
    ///
    /// Not surface.accent, which is what the status header used to paint.
    /// Every shipped theme gives surface.accent and eye.alert the same
    /// hex and the same 256 fallback, so an accent-painted mark was the
    /// alert eye in every respect a user can see.
    pub fn eye(&self, calm: bool, text: &str) -> String {
        let token = if calm {
            tokens::EYE_CALM
        } else {
            tokens::EYE_ALERT
        };
        self.paint(self.theme.resolve(token), text)
    }

    /// A state cell in its group's color: healthy, needs-you, terminal,
    /// quiet, or the step below quiet for a dead pane.
    ///
    /// Redundant, never alone (GOALS). The cell always carries the glyph
    /// and the word, so this adds nothing a `--plain` or `NO_COLOR` reader
    /// loses; what it buys a sighted reader is the two questions the glyph
    /// makes them decode. Is this row mine, and will it clear itself?
    pub fn state(&self, state: AgentState, text: &str) -> String {
        self.paint(self.theme.resolve(cyclops_theme::state_token(state)), text)
    }

    /// A delivery badge in its group's color, the same four groups read on
    /// a delivery instead of a pane.
    pub fn badge(&self, state: DeliveryState, text: &str) -> String {
        self.paint(
            self.theme.resolve(cyclops_theme::delivery_token(state)),
            text,
        )
    }

    /// Dim: detail columns, gutters, separators.
    pub fn dim(&self, text: &str) -> String {
        self.paint(self.theme.resolve(tokens::SURFACE_DIM), text)
    }

    /// The accent: the marker on the row a surface is pointing at. The
    /// stream paints its selected entry with it (cyclops-ui frame.rs) and
    /// `cyclops theme` marks the active theme with the same glyph in the
    /// same token, so the mark means one thing on both surfaces.
    pub fn accent(&self, text: &str) -> String {
        self.paint(self.theme.resolve(tokens::SURFACE_ACCENT), text)
    }

    pub fn bold(&self, text: &str) -> String {
        if !self.color {
            return text.to_string();
        }
        format!("\x1b[1m{text}\x1b[0m")
    }
}

/// The CLI's half of the shared cell vocabulary. The methods above already
/// do the work; this is what lets `cyclops_ui::grid` compose a cell once
/// for both surfaces.
impl cyclops_ui::grid::Paint for Style {
    fn dim(&self, text: &str) -> String {
        Style::dim(self, text)
    }

    fn state(&self, state: AgentState, text: &str) -> String {
        Style::state(self, state, text)
    }

    fn badge(&self, state: DeliveryState, text: &str) -> String {
        Style::badge(self, state, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte parity with the stream: the same crate, the same hash, the
    /// same slot table. A drift here re-colors agents between surfaces.
    #[test]
    fn role_paint_uses_the_engine_hash() {
        let s = Style {
            color: true,
            truecolor: true,
            theme: Theme::default(),
        };
        let slot = cyclops_theme::role_slot("reviewer");
        let (r, g, b) = Theme::default().resolve(tokens::ROLE[slot]).rgb;
        assert!(s
            .role("reviewer", "reviewer")
            .starts_with(&format!("\x1b[38;2;{r};{g};{b}m")));
    }

    #[test]
    fn truecolor_and_256_sequences() {
        let tc = Style {
            color: true,
            truecolor: true,
            theme: Theme::default(),
        };
        let lo = Style {
            color: true,
            truecolor: false,
            theme: Theme::default(),
        };
        // The compiled default table keeps the pre-theme palette: dim is
        // still 128,128,128 / 245 with no theme file anywhere.
        assert!(tc.dim("x").starts_with("\x1b[38;2;128;128;128m"));
        assert!(lo.dim("x").starts_with("\x1b[38;5;245m"));
        assert!(tc.dim("x").ends_with("\x1b[0m"));
    }

    #[test]
    fn no_color_paths_return_text_untouched() {
        let s = Style::none();
        assert_eq!(s.role("reviewer", "reviewer"), "reviewer");
        assert_eq!(s.dim("x"), "x");
        assert_eq!(s.bold("x"), "x");
        assert_eq!(s.eye(true, "‿"), "‿");
        assert_eq!(s.eye(false, "◉"), "◉");
    }

    #[test]
    fn the_eye_paints_through_the_eye_tokens() {
        // A theme that moves the eye tokens must move the status mark,
        // and calm must not resolve to the alert color. The shipped
        // themes make surface.accent equal to eye.alert, so painting the
        // mark through accent looked correct while alarming on a calm rig.
        let (theme, warnings) = Theme::parse(
            "[eye]\ncalm = { hex = \"#010203\", c256 = 17 }\n\
             alert = { hex = \"#040506\", c256 = 18 }\n",
            "test",
        )
        .expect("parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        let tc = Style {
            color: true,
            truecolor: true,
            theme: theme.clone(),
        };
        let lo = Style {
            color: true,
            truecolor: false,
            theme,
        };
        assert!(tc.eye(true, "‿").starts_with("\x1b[38;2;1;2;3m"));
        assert!(tc.eye(false, "◉").starts_with("\x1b[38;2;4;5;6m"));
        assert!(lo.eye(true, "‿").starts_with("\x1b[38;5;17m"));
        assert!(lo.eye(false, "◉").starts_with("\x1b[38;5;18m"));
    }

    /// The state cell and the badge resolve through the engine's groups.
    /// A theme that moves a group has to move every state in it, and the
    /// five slots must not collapse onto one color. Which state belongs
    /// to which group is cyclops-theme's rule and is pinned there; this
    /// pins that the CLI asks, instead of carrying a palette again.
    #[test]
    fn state_and_badge_paint_through_the_group_tokens() {
        let (theme, warnings) = Theme::parse(
            concat!(
                "[state]\n",
                "healthy = { hex = \"#010101\", c256 = 11 }\n",
                "needs_you = { hex = \"#020202\", c256 = 12 }\n",
                "terminal = { hex = \"#030303\", c256 = 13 }\n",
                "quiet = { hex = \"#040404\", c256 = 14 }\n",
                "dead = { hex = \"#050505\", c256 = 15 }\n",
                "[badge]\n",
                "healthy = { hex = \"#060606\", c256 = 16 }\n",
                "needs_you = { hex = \"#070707\", c256 = 17 }\n",
                "terminal = { hex = \"#080808\", c256 = 18 }\n",
                "quiet = { hex = \"#090909\", c256 = 19 }\n",
            ),
            "test",
        )
        .expect("parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        let s = Style::with_theme(theme, false);

        for (state, want) in [
            (AgentState::Working, 11),
            (AgentState::BlockedModal, 12),
            (AgentState::BlockedPermission, 12),
            (AgentState::BlockedQuota, 13),
            (AgentState::Idle, 14),
            (AgentState::IdleWithInput, 14),
            (AgentState::Unknown, 14),
            (AgentState::Dead, 15),
        ] {
            assert!(
                s.state(state, "x")
                    .starts_with(&format!("\x1b[38;5;{want}m")),
                "{state} painted {:?}",
                s.state(state, "x")
            );
        }
        for (state, want) in [
            (DeliveryState::DeliveredVerified, 16),
            (DeliveryState::DeliveredUnverified, 16),
            (DeliveryState::AttentionRequired, 17),
            (DeliveryState::ParkedBlockedQuota, 18),
            (DeliveryState::Queued, 19),
            (DeliveryState::Gating, 19),
            (DeliveryState::Pasting, 19),
            (DeliveryState::Staged, 19),
            (DeliveryState::Submitted, 19),
            (DeliveryState::RetryQueued, 19),
        ] {
            assert!(
                s.badge(state, "x")
                    .starts_with(&format!("\x1b[38;5;{want}m")),
                "{state:?} painted {:?}",
                s.badge(state, "x")
            );
        }
    }

    /// Color off loses nothing: the same words, and not one escape byte.
    /// This is the whole reason state color is allowed to exist at all.
    #[test]
    fn color_off_leaves_state_and_badge_untouched() {
        let s = Style::none();
        // Glyph-free on purpose: the paint is a passthrough, and pinning
        // a glyph here would be a copy of the vocabulary grid.rs owns.
        assert_eq!(
            s.state(AgentState::BlockedQuota, "blocked_quota"),
            "blocked_quota"
        );
        assert_eq!(
            s.badge(DeliveryState::ParkedBlockedQuota, "parked"),
            "parked"
        );
    }

    #[test]
    fn a_loaded_theme_changes_the_paint() {
        let (theme, warnings) = Theme::parse(
            "[surface]\ndim = { hex = \"#102030\", c256 = 17 }\n",
            "test",
        )
        .expect("parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        let tc = Style {
            color: true,
            truecolor: true,
            theme: theme.clone(),
        };
        let lo = Style {
            color: true,
            truecolor: false,
            theme,
        };
        assert!(tc.dim("x").starts_with("\x1b[38;2;16;32;48m"));
        assert!(lo.dim("x").starts_with("\x1b[38;5;17m"));
        // Tokens the theme does not set keep the compiled defaults.
        assert!(tc.eye(false, "x").starts_with("\x1b[38;2;209;154;102m"));
    }
}
