//! Ratatui styles from cyclops-theme tokens.

use std::io::IsTerminal;

use cyclops_theme::{tokens, Color as ThemeColor, Theme};
use ratatui::style::{Color as RtColor, Modifier, Style};

/// Workspace paint context.
pub struct Paint {
    pub theme: Theme,
    pub truecolor: bool,
    colors_enabled: bool,
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
            colors_enabled: color,
        }
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Paint {
            theme: Theme::default(),
            truecolor: false,
            colors_enabled: true,
        }
    }

    #[cfg(test)]
    fn without_color_for_test() -> Self {
        Paint {
            theme: Theme::default(),
            truecolor: false,
            colors_enabled: false,
        }
    }

    pub fn style_token(&self, token: &str) -> Style {
        if !self.colors_enabled {
            return Style::new();
        }
        Style::new().fg(rt_color(self.theme.resolve(token), self.truecolor))
    }

    /// A style whose background is the token's color — the chrome grounds.
    pub fn bg_token(&self, token: &str) -> Style {
        if !self.colors_enabled {
            return Style::new();
        }
        Style::new().bg(rt_color(self.theme.resolve(token), self.truecolor))
    }

    pub fn role(&self, label: &str) -> Style {
        if !self.colors_enabled {
            return Style::new();
        }
        Style::new().fg(rt_color(self.theme.role(label), self.truecolor))
    }

    pub fn state(&self, state: cyclops_proto::AgentState) -> Style {
        self.style_token(cyclops_theme::state_token(state))
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

/// The chrome ground: the tab strip, pane gutters, menu bodies.
pub fn chrome_panel(paint: &Paint) -> Style {
    paint
        .style_token(tokens::CHROME_TEXT)
        .patch(paint.bg_token(tokens::CHROME_PANEL))
}

/// One step up from the panel: the active chip, hovered rows, selection.
pub fn chrome_raised(paint: &Paint) -> Style {
    paint
        .style_token(tokens::CHROME_TEXT)
        .patch(paint.bg_token(tokens::CHROME_RAISED))
}

pub fn tab_active(paint: &Paint) -> Style {
    accent_fill(paint)
}

pub fn tab_inactive(paint: &Paint) -> Style {
    paint
        .style_token(tokens::SURFACE_DIM)
        .patch(paint.bg_token(tokens::CHROME_PANEL))
}

pub fn sidebar_row_active(paint: &Paint) -> Style {
    chrome_raised(paint)
}

pub fn sidebar_workspace(paint: &Paint) -> Style {
    chrome_panel(paint).add_modifier(Modifier::BOLD)
}

pub fn sidebar_workspace_active(paint: &Paint) -> Style {
    chrome_raised(paint).add_modifier(Modifier::BOLD)
}

pub fn sidebar_row(paint: &Paint) -> Style {
    paint
        .style_token(tokens::SURFACE_DIM)
        .patch(paint.bg_token(tokens::CHROME_PANEL))
}

/// The grabbed row while a sidebar-row reorder drag is live. Dim is the
/// color cue; the caller also swaps in a grip glyph (see
/// `render::paint_sidebar`) so the row still reads as "being moved" with
/// `NO_COLOR` — rule 11 wants a non-color encoding beside the color one,
/// not instead of it.
pub fn sidebar_row_dragging(paint: &Paint) -> Style {
    sidebar_row(paint).add_modifier(Modifier::DIM)
}

/// The live insertion rule a sidebar workspace-row drag previews. Reuses
/// the same accent token as a focused pane border rather than inventing a
/// new one for this one purpose.
pub fn drag_insertion_rule(paint: &Paint) -> Style {
    pane_border_focused(paint)
}

pub fn sidebar_label(paint: &Paint) -> Style {
    paint
        .style_token(tokens::SURFACE_DIM)
        .patch(paint.bg_token(tokens::CHROME_PANEL))
}

/// Compact create buttons shared by the tab strip and sidebar footer.
pub fn add_button(paint: &Paint) -> Style {
    chrome_raised(paint)
        .patch(paint.style_token(tokens::SURFACE_ACCENT))
        .add_modifier(Modifier::BOLD)
}

/// The create button under the mouse. It fills rather than merely
/// brightening, so the glyph reads as a button the moment it is pointed
/// at instead of only once it has been clicked.
pub fn add_button_hover(paint: &Paint) -> Style {
    accent_fill(paint)
}

/// Primary keyboard action in a modal and the selected tab chip.
pub fn accent_fill(paint: &Paint) -> Style {
    if paint.colors_enabled {
        paint
            .style_token(tokens::CHROME_PANEL)
            .patch(paint.bg_token(tokens::SURFACE_ACCENT))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD)
    }
}

pub fn dialog_primary(paint: &Paint) -> Style {
    accent_fill(paint)
}

pub fn dialog_secondary(paint: &Paint) -> Style {
    chrome_panel(paint).add_modifier(Modifier::BOLD)
}

/// A menu or dialog surface at rest. Overlays sit one level above the
/// workspace panel so they read as theme furniture rather than black holes.
pub fn menu_row(paint: &Paint) -> Style {
    chrome_raised(paint)
}

/// The menu or dialog row under the mouse.
pub fn menu_row_hover(paint: &Paint) -> Style {
    if paint.colors_enabled {
        paint
            .style_token(tokens::SURFACE_ACCENT)
            .patch(paint.bg_token(tokens::CHROME_PANEL))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::REVERSED)
    }
}

/// Secondary copy on a raised menu or dialog surface.
pub fn menu_hint(paint: &Paint) -> Style {
    paint
        .style_token(tokens::SURFACE_DIM)
        .patch(paint.bg_token(tokens::CHROME_RAISED))
}

/// Inset text field within a raised dialog.
pub fn dialog_input(paint: &Paint) -> Style {
    chrome_panel(paint)
}

pub fn dialog_error(paint: &Paint) -> Style {
    paint
        .style_token(tokens::STATE_TERMINAL)
        .patch(paint.bg_token(tokens::CHROME_RAISED))
}

/// Pane body uses the terminal's own foreground; see docs/themes.md.
pub fn pane_cell(_paint: &Paint) -> Style {
    Style::new()
}

pub fn attention_eye(paint: &Paint) -> Style {
    paint.style_token(tokens::EYE_ALERT)
}

pub fn selection_highlight(paint: &Paint) -> Style {
    if paint.colors_enabled {
        paint
            .style_token(tokens::SURFACE_ACCENT)
            .patch(paint.bg_token(tokens::CHROME_RAISED))
    } else {
        Style::new().add_modifier(Modifier::REVERSED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_disables_tokens_but_keeps_interaction_visible() {
        let paint = Paint::without_color_for_test();
        assert_eq!(chrome_panel(&paint), Style::new());
        assert_eq!(pane_border_focused(&paint), Style::new());
        assert_eq!(
            menu_row_hover(&paint),
            Style::new().add_modifier(Modifier::REVERSED)
        );
        assert_eq!(
            selection_highlight(&paint),
            Style::new().add_modifier(Modifier::REVERSED)
        );
    }
}
