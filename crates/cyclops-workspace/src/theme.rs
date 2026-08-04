//! Ratatui styles from cyclops-theme tokens.

use std::io::IsTerminal;

use cyclops_theme::{tokens, Color as ThemeColor, Theme};
use ratatui::style::{Color as RtColor, Style};

/// Workspace paint context.
pub struct Paint {
    pub theme: Theme,
    pub truecolor: bool,
}

impl Paint {
    pub fn detect() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let color = !no_color && std::io::stdout().is_terminal();
        let theme = if color {
            let sel = cyclops_theme::active(&cyclops_proto::cyclops_home());
            for w in &sel.warnings {
                eprintln!("theme: {w}");
            }
            sel.theme
        } else {
            Theme::default()
        };
        Paint {
            theme,
            truecolor: std::env::var("COLORTERM").as_deref() == Ok("truecolor"),
        }
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Paint {
            theme: Theme::default(),
            truecolor: false,
        }
    }

    pub fn style_token(&self, token: &str) -> Style {
        Style::new().fg(rt_color(self.theme.resolve(token), self.truecolor))
    }
}

fn rt_color(c: ThemeColor, truecolor: bool) -> RtColor {
    if truecolor {
        let (r, g, b) = c.rgb;
        RtColor::Rgb(r, g, b)
    } else {
        RtColor::Indexed(c.c256)
    }
}

pub fn pane_border_focused(paint: &Paint) -> Style {
    paint.style_token(tokens::SURFACE_ACCENT)
}

pub fn pane_border(paint: &Paint) -> Style {
    paint.style_token(tokens::SURFACE_DIM)
}

pub fn tab_active(paint: &Paint) -> Style {
    paint.style_token(tokens::SURFACE_ACCENT)
}

pub fn tab_inactive(paint: &Paint) -> Style {
    paint.style_token(tokens::SURFACE_DIM)
}

pub fn sidebar_row_active(paint: &Paint) -> Style {
    paint.style_token(tokens::SURFACE_ACCENT)
}

pub fn sidebar_row(paint: &Paint) -> Style {
    paint.style_token(tokens::SURFACE_DIM)
}

pub fn sidebar_label(paint: &Paint) -> Style {
    paint.style_token(tokens::SURFACE_DIM)
}

/// Pane body uses the terminal's own foreground; see docs/themes.md.
pub fn pane_cell(_paint: &Paint) -> Style {
    Style::new()
}

pub fn attention_eye(paint: &Paint) -> Style {
    paint.style_token(tokens::EYE_ALERT)
}

pub fn selection_highlight(paint: &Paint) -> Style {
    paint
        .style_token(tokens::SURFACE_ACCENT)
        .bg(RtColor::Indexed(236))
}
