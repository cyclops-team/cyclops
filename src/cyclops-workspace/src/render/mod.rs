//! Frame composition and render-derived hit geometry: painting panes and
//! chrome into a Ratatui buffer, and the top-level chrome layout every
//! surface below is painted into.
//!
//! Visible pane cells render 1:1 from the runtime's leading viewport. At the
//! sizing driver's extent, pane geometry is cell-exact; a smaller follower
//! may proportionally fit local card bounds to reserve chrome, but never
//! scales runtime cells or changes the shared tmux window.
//!
//! This module does not own persistence, daemon queries, or attention
//! predicates — it reads whatever state its callers hand it and paints or
//! measures, nothing more. Each surface below has clear seams (sidebar
//! with its Sessions/Stream tabs, pane canvas, tab bar, dialogs/menus)
//! and lives in its own file; this file owns only what those surfaces
//! share: the top-level chrome split (`chrome_areas_for`), the
//! cell-to-style bridge (`cell_style`/`rt_color`), and the one text
//! primitive (`overlay_text`) every surface paints through.
//!
//! One rule places every visible control, so the chrome reads as one
//! language rather than a pile of idioms:
//!
//! 1. A control lives in the chrome of the thing it acts on, painted as a
//!    single glyph that says which way the click moves things, with a hit
//!    target as large as that chrome allows and a fill under the mouse
//!    (`theme::add_button` / `add_button_hover`). The tab strip's `+`, the
//!    sidebar footer's `+`, the sidebar chevron, and the Messages pane's
//!    footer buttons are all this.
//! 2. When a surface is put away it has no chrome left to host its own
//!    switch, so the app menu carries it, and the app menu stays
//!    reachable because a collapsed sidebar keeps a rail
//!    (`SIDEBAR_RAIL_WIDTH`) rather than disappearing.
//! 3. Feedback is not a control: the transient notice
//!    (`crate::notice`, painted by `canvas::paint_notice`) states a fact,
//!    registers no hit target, and moves no rectangle.

#![allow(clippy::too_many_arguments)]

mod canvas;
mod files;
mod overlay;
mod sidebar;
mod stream;
mod tab_bar;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};
use ratatui::text::Span;

use crate::drag::{DragState, DragTarget};
use crate::runtime::{Color, GridCell};
use crate::theme::Paint;

pub use canvas::{
    paint_window, tmux_client_size, window_target_size_for_layout, HostCursor, WindowPaintCtx,
    MINIMIZED_ROWS, PANE_GRIP,
};
/// For the arithmetic check in
/// `app::tests::narrowing_the_sidebar_strands_canvas_columns_until_tmux_is_told`,
/// which adds the panel, the declared grid, these two, and the layout's
/// gap overhead back up to the terminal's width. Nothing outside a test
/// needs them: every caller that spends these cells is inside `canvas`.
#[cfg(test)]
pub use canvas::{PANE_GAPS, PANE_MARGIN};
pub use overlay::{clamp_dialog_offset, keybind_max_scroll, paint_dialog, paint_menu, MenuChecks};
pub use sidebar::{
    paint_daemon_status, paint_messages, paint_messages_rail, paint_messages_resize_feedback,
    paint_sidebar_filtered, paint_sidebar_rail, paint_sidebar_resize_feedback, sidebar_body_bottom,
    MessagesRailCue, SIDEBAR_COLLAPSE, SIDEBAR_EXPAND,
};
pub use stream::{event_stream_rows, EventRow};
pub use tab_bar::paint_tab_bar;

/// A chrome region's height in the tab bar. It never grows: the row is a
/// strip, not a panel.
const TAB_BAR_HEIGHT: u16 = 1;
/// The floor a sidebar can be dragged to. Below it there is nowhere left to
/// cut: the panel would be all chrome and no content, so collapsing to the
/// one-column rail is the real "smaller than this," not a thinner panel.
/// At the floor itself the panel still shows every control whole — the
/// full "☰menu" and `+` fit the footer, both tab chips paint
/// (ellipsized), and a workspace or agent name keeps a few cells before
/// its own ellipsis — so it keeps doing its job in less room rather than
/// degenerating into noise. A terminal narrow enough for the half-width
/// cap in `clamp_sidebar_width` to undercut this floor can still produce
/// a thinner panel, which is what the narrowest paint paths (the footer's
/// glyph-only menu button) remain for.
pub(crate) const SIDEBAR_MIN_WIDTH: u16 = 14;
/// The width a fresh install opens the sidebar at. The minimum above is no
/// longer this width — it is how far an operator may narrow the panel by
/// dragging, not where it starts.
pub(crate) const SIDEBAR_DEFAULT_WIDTH: u16 = 24;
/// Widest a sidebar may grow before it starts crowding the pane canvas it
/// exists to introduce.
const SIDEBAR_MAX_WIDTH: u16 = 42;
/// What a collapsed sidebar leaves behind: one column, carrying the
/// chevron that brings the panel back and nothing else. Collapsing must
/// not strand the mouse, because the panel's footer holds the only pointer
/// route to the app menu, so the rail is the way back and the canvas gets
/// every column the panel gave up except this one.
pub(crate) const SIDEBAR_RAIL_WIDTH: u16 = 1;

/// Geometry constants for the right-edge Messages pane.
pub(crate) const MESSAGES_MIN_WIDTH: u16 = 14;
pub(crate) const MESSAGES_DEFAULT_WIDTH: u16 = 24;
pub(crate) const MESSAGES_RAIL_WIDTH: u16 = 1;
/// The Messages pane carries whole conversations, not a list of names, so
/// how wide it should be is the operator's judgement and not a number chosen
/// here: a fixed ceiling truncated headers and the action strip mid-word on
/// a wide terminal, with no way to widen past it. The pane owns a peer region
/// beside the agent canvas, and this lower bound keeps that canvas usable.
pub(crate) const MAIN_MIN_WIDTH: u16 = 20;

/// Chrome rectangles for one frame. `sidebar` and `rail` are mutually
/// exclusive on the left edge; `messages` and `messages_rail` are mutually
/// exclusive on the right edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeAreas {
    pub sidebar: Option<Rect>,
    pub rail: Option<Rect>,
    pub tab_bar: Rect,
    pub canvas: Rect,
    pub messages: Option<Rect>,
    pub messages_rail: Option<Rect>,
}

impl ChromeAreas {
    /// Agent canvas used to derive the tmux window size.
    ///
    /// It is intentionally identical to the painted canvas. Child TUIs must
    /// receive the width left after Messages reserves its peer region so they
    /// wrap at the visible pane edge instead of rendering behind the panel.
    pub fn tmux_sizing_canvas(&self) -> Rect {
        self.canvas
    }
}

/// Split one frame into the sidebar (or its rail), Messages pane (or its
/// rail), tab bar, and agent pane canvas: the top-level chrome composition
/// every painted surface below sits inside. `app` decides visibility and
/// width; this only turns those decisions into rectangles.
pub fn chrome_areas_for(
    area: Rect,
    sidebar_visible: bool,
    sidebar_width: u16,
    tab_bar_visible: bool,
    messages_visible: bool,
    messages_width: u16,
) -> ChromeAreas {
    let mut main = area;
    let sidebar = if sidebar_visible && main.width > 4 {
        let w = clamp_sidebar_width(sidebar_width, main.width);
        let s = Rect::new(main.x, main.y, w, main.height);
        main = Rect::new(main.x + w, main.y, main.width - w, main.height);
        Some(s)
    } else {
        None
    };
    // Only a genuine collapse leaves a rail. A terminal too narrow for the
    // panel at all has no room to offer a control either, and painting a
    // "reopen" chevron for a sidebar that is already meant to be open
    // would lie about what the click does.
    let rail = if !sidebar_visible && main.width > SIDEBAR_RAIL_WIDTH {
        let r = Rect::new(main.x, main.y, SIDEBAR_RAIL_WIDTH, main.height);
        main = Rect::new(
            main.x + SIDEBAR_RAIL_WIDTH,
            main.y,
            main.width - SIDEBAR_RAIL_WIDTH,
            main.height,
        );
        Some(r)
    } else {
        None
    };
    let messages = if messages_visible && main.width > 4 {
        let w = clamp_messages_width(messages_width, main.width);
        let m = Rect::new(main.x + main.width - w, main.y, w, main.height);
        main = Rect::new(main.x, main.y, main.width - w, main.height);
        Some(m)
    } else {
        None
    };
    let messages_rail = if !messages_visible && main.width > MESSAGES_RAIL_WIDTH {
        let r = Rect::new(
            main.x + main.width - MESSAGES_RAIL_WIDTH,
            main.y,
            MESSAGES_RAIL_WIDTH,
            main.height,
        );
        main = Rect::new(
            main.x,
            main.y,
            main.width - MESSAGES_RAIL_WIDTH,
            main.height,
        );
        Some(r)
    } else {
        None
    };
    let bar_h = if tab_bar_visible {
        TAB_BAR_HEIGHT.min(main.height)
    } else {
        0
    };
    let tab_bar = Rect::new(main.x, main.y, main.width, bar_h);
    let canvas = Rect::new(
        main.x,
        main.y + bar_h,
        main.width,
        main.height.saturating_sub(bar_h),
    );
    ChromeAreas {
        sidebar,
        rail,
        tab_bar,
        canvas,
        messages,
        messages_rail,
    }
}

/// Bound a requested sidebar width to what stays readable without eating
/// more than half the terminal.
pub fn clamp_sidebar_width(requested: u16, terminal_width: u16) -> u16 {
    let max = SIDEBAR_MAX_WIDTH.min(terminal_width / 2).max(1);
    let min = SIDEBAR_MIN_WIDTH.min(max);
    requested.clamp(min, max)
}

/// Bound a requested Messages pane width to what leaves the agent pane
/// canvas usable. The operator decides how wide the conversation is; this
/// keeps both peer regions present.
pub fn clamp_messages_width(requested: u16, terminal_width: u16) -> u16 {
    let max = terminal_width.saturating_sub(MAIN_MIN_WIDTH).max(1);
    let min = MESSAGES_MIN_WIDTH.min(max);
    requested.clamp(min, max)
}

/// The sidebar width a live drag to `column` would commit, bounded the same
/// way a resting preference is.
pub fn sidebar_width_for_column(column: u16, terminal_width: u16) -> u16 {
    clamp_sidebar_width(column, terminal_width)
}

/// The Messages pane width a live drag from the right edge to `column` would
/// commit.
pub fn messages_width_for_column(column: u16, terminal_width: u16) -> u16 {
    let width_from_right = terminal_width.saturating_sub(column);
    clamp_messages_width(width_from_right, terminal_width)
}

/// The width to restore when a sidebar-resize drag is cancelled: `None` for
/// every other drag target, which has nothing here to restore.
pub fn sidebar_width_on_cancel(drag: &DragState, terminal_width: u16) -> Option<u16> {
    matches!(&drag.target, DragTarget::Sidebar)
        .then(|| sidebar_width_for_column(drag.start.0, terminal_width))
}

/// The width to restore when a Messages pane drag is cancelled.
pub fn messages_width_on_cancel(drag: &DragState, terminal_width: u16) -> Option<u16> {
    matches!(&drag.target, DragTarget::Messages)
        .then(|| messages_width_for_column(drag.start.0, terminal_width))
}

/// Write `text` onto one row, clipped to `bounds`.
fn overlay_text(buf: &mut Buffer, bounds: Rect, x: u16, y: u16, text: &str, style: Style) {
    if y < bounds.y || y >= bounds.y + bounds.height || x < bounds.x || x >= bounds.x + bounds.width
    {
        return;
    }
    let width = (bounds.x + bounds.width - x) as usize;
    buf.set_stringn(x, y, text, width, style);
}

/// `overlay_text`'s sibling for anything long enough to outgrow a narrow
/// sidebar: text that overflows the space from `x` to `bounds`'s right edge
/// ends in `…` instead of being hard-clipped mid-word. A workspace or agent
/// name that used to chop into an unreadable stub now reads "my-proj…", the
/// same way a browser tab or a file manager shortens a name that does not
/// fit.
///
/// Unicode-width aware, the same way the rest of this module measures text
/// (`Span::raw(...).width()`): the fit check and the truncation budget are
/// both in display columns, not bytes or chars, and the cut point never
/// lands inside a wide glyph's own pair of cells — a glyph that would not
/// fit whole is dropped whole, and `…` takes its place.
fn overlay_text_ellipsized(
    buf: &mut Buffer,
    bounds: Rect,
    x: u16,
    y: u16,
    text: &str,
    style: Style,
) {
    if y < bounds.y || y >= bounds.y + bounds.height || x < bounds.x || x >= bounds.x + bounds.width
    {
        return;
    }
    let available = (bounds.x + bounds.width - x) as usize;
    if Span::raw(text).width() <= available {
        overlay_text(buf, bounds, x, y, text, style);
        return;
    }
    if available == 1 {
        buf.set_stringn(x, y, "…", 1, style);
        return;
    }
    // Keep whole chars up to the budget the trailing `…` reserves for
    // itself, so the cut always lands on a char boundary and a wide glyph
    // that would land half in, half out is skipped rather than split.
    let budget = available - 1;
    let mut kept_width = 0usize;
    let mut kept_bytes = 0usize;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if kept_width + w > budget {
            break;
        }
        kept_width += w;
        kept_bytes += ch.len_utf8();
    }
    let mut shown = text[..kept_bytes].to_string();
    shown.push('…');
    buf.set_stringn(x, y, &shown, available, style);
}

fn cell_style(cell: &GridCell, base: Style, palette: Option<&[RtColor; 16]>) -> Style {
    let mut style = base;
    if let Some(fg) = rt_color(cell.attrs.fg, palette) {
        style = style.fg(fg);
    }
    if let Some(bg) = rt_color(cell.attrs.bg, palette) {
        style = style.bg(bg);
    }
    // Colors-on adjustments (a palette exists exactly when colors are
    // on). Agents pick their colors for the terminal THEY imagine. Two
    // symptoms, one cause: a neutral fill at the far luminance extreme
    // (codex's #393939 composer, painted for the dark ground tmux
    // reported) re-grounds to this theme's own panel, and text that
    // cannot read on its ground clamps to the floor. DIM folds into the
    // clamp's math rather than passing to the terminal, which would
    // halve brightness AFTER the clamp. Block, shade, and braille
    // glyphs carry image pixels, not text: their colors pass through
    // untouched, DIM included. Rule 11 holds because none of this runs
    // with color off.
    let pixels = paints_pixels(cell.ch);
    if palette.is_some() && !pixels {
        if !cell.attrs.reverse {
            if let (Some(bg), Some(ground)) = (style.bg, base.bg) {
                if let Some(panel) = matched_ground(bg, ground, base.fg, palette) {
                    style.bg = Some(panel);
                } else if let Some(flipped) = mirrored_tint(bg, ground, palette) {
                    // A diff band and the like: keep the hue, move the
                    // lightness to this theme's side of the ground.
                    style.bg = Some(flipped);
                }
            }
        }
        if let (Some(fg), Some(bg)) = (style.fg, style.bg) {
            style.fg = Some(readable_fg(fg, bg, cell.attrs.dim, palette));
        }
    }
    if cell.attrs.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.attrs.dim && (palette.is_none() || pixels) {
        style = style.add_modifier(Modifier::DIM);
    }
    if cell.attrs.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.attrs.underline.is_underlined() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.attrs.reverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if cell.attrs.strikeout {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if cell.attrs.hidden {
        style = style.add_modifier(Modifier::HIDDEN);
    }
    style
}

fn rt_color(c: Color, palette: Option<&[RtColor; 16]>) -> Option<RtColor> {
    match c {
        // The themed pane ground wins where the program set nothing.
        Color::Default => None,
        // ANSI 0..15 map through the theme's palette when one is on;
        // 16..255 and the paletteless path stay the host's own.
        Color::Indexed(i) => Some(match palette {
            Some(p) if usize::from(i) < p.len() => p[usize::from(i)],
            _ => RtColor::Indexed(i),
        }),
        Color::Rgb(r, g, b) => Some(RtColor::Rgb(r, g, b)),
    }
}

/// The readability floor a pane cell's text never falls under. Body-text
/// bars belong to the theme's own figures; this only catches pairs an
/// agent composed for a different ground, so it sits well below them.
const MIN_CONTRAST: f64 = 3.0;

/// How far a DIM cell's text fades toward its background before the
/// floor catches it. The fade is applied here, in the same math the
/// clamp measures, never as the terminal's own DIM.
const DIM_FADE: f64 = 0.4;

/// Fills at or past these luminance extremes read as "the ground the
/// program thought it had": near-black painted for a dark terminal,
/// near-white for a light one. Dark anchor, measured: codex's dark-mode
/// composer paints #393939 (luminance 0.041); Claude Code's command
/// bars sit lower still. The bounds are asymmetric because luminance is:
/// light grounds cluster from #dcdcdc (0.72) up, while #d0d0d0 (0.63)
/// is already a mid gray no app uses as paper.
const FOREIGN_DARK_MAX_L: f64 = 0.10;
const FOREIGN_LIGHT_MIN_L: f64 = 0.70;

/// Only neutral fills re-ground: channel spread at or under this. A
/// chromatic dark (a diff's green fill, a powerline segment) is
/// content, not a mistaken ground, and keeps its color.
const NEUTRAL_SPREAD_MAX: u8 = 24;
/// How far a TINTED fill's lightness has to be from the theme's ground
/// before it counts as painted for the other kind of terminal. Looser than
/// the neutral thresholds beside it, because a diff band is deliberately
/// not black: a dark-theme red sits around 0.10 to 0.25, and holding it to
/// 0.10 would leave every one of them dark on a light theme.
const FOREIGN_TINT_DARK_MAX_L: f64 = 0.30;
const FOREIGN_TINT_LIGHT_MIN_L: f64 = 0.62;

/// How far the replacement panel leans from the ground toward the ink:
/// enough to keep a composer box visible as a box, no more.
const PANEL_TINT: f64 = 0.10;

/// `fg`, faded when `dim` and then nudged toward black or white until it
/// clears [`MIN_CONTRAST`] against `bg`. A pair already readable comes
/// back with its hue (dim included, as a fade toward the ground rather
/// than the terminal's blind darkening); a hopeless one lands on the
/// pole. The output is truecolor when the palette resolves truecolor
/// (the theme's own signal) and a 256-color grid entry otherwise, so a
/// clamped color never emits a sequence the terminal was told not to
/// expect.
fn readable_fg(fg: RtColor, bg: RtColor, dim: bool, palette: Option<&[RtColor; 16]>) -> RtColor {
    let (Some(mut f), Some(b)) = (srgb(fg), srgb(bg)) else {
        return fg;
    };
    if dim {
        f = lerp(f, b, DIM_FADE);
    }
    if contrast(f, b) >= MIN_CONTRAST {
        return if dim { emit(f, palette) } else { fg };
    }
    let pole = if luminance(b) > 0.5 {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    };
    let mut fixed = pole;
    for step in [0.25, 0.5, 0.75] {
        let c = lerp(f, pole, step);
        if contrast(c, b) >= MIN_CONTRAST {
            fixed = c;
            break;
        }
    }
    emit(fixed, palette)
}

/// The theme's own panel color for a fill painted against the wrong
/// ground, or `None` for every fill that is the program's business. A
/// neutral background at the luminance extreme opposite this theme's
/// ground was composed for the terminal the program imagined: tmux
/// reports the ground of whichever real client taught it, so an agent
/// under the user's dark terminal paints dark fills into a light
/// workspace. Detection cannot be fixed at the source, because the same
/// pane is viewed through that dark terminal AND this theme at once;
/// the restyle happens here, per theme, at render.
fn matched_ground(
    bg: RtColor,
    ground: RtColor,
    ink: Option<RtColor>,
    palette: Option<&[RtColor; 16]>,
) -> Option<RtColor> {
    let (Some(b), Some(g)) = (srgb(bg), srgb(ground)) else {
        return None;
    };
    let spread = b.0.max(b.1).max(b.2) - b.0.min(b.1).min(b.2);
    if spread > NEUTRAL_SPREAD_MAX {
        return None;
    }
    let (bl, gl) = (luminance(b), luminance(g));
    let foreign = if gl >= 0.5 {
        bl <= FOREIGN_DARK_MAX_L
    } else {
        bl >= FOREIGN_LIGHT_MIN_L
    };
    if !foreign {
        return None;
    }
    let pole = if gl >= 0.5 {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    };
    let ink = ink.and_then(srgb).unwrap_or(pole);
    Some(emit(lerp(g, ink, PANEL_TINT), palette))
}

/// A tinted fill from the other kind of terminal: a diff's red or green
/// band, chosen for a dark ground and still dark under a light theme.
///
/// [`matched_ground`] deliberately refuses these, and it is right to. It
/// re-grounds NEUTRAL fills, where the exact grey the agent picked means
/// nothing and the theme's own panel is a better answer. A tinted band is
/// the opposite case: the hue is the information. Replacing a diff's red
/// with the panel color deletes the diff.
///
/// So hue and saturation survive untouched and only lightness moves. The
/// color is mirrored across mid-lightness, which turns a dark red into a
/// light red: still obviously the removed side, now legible on a light
/// ground. Under a dark theme the same mirror pulls a band painted for a
/// light terminal back down.
///
/// Fires only for a fill that is genuinely foreign, meaning its lightness
/// sits at the far pole from the theme's own ground. A band already on the
/// right side of the ground is left exactly as the agent drew it.
fn mirrored_tint(bg: RtColor, ground: RtColor, palette: Option<&[RtColor; 16]>) -> Option<RtColor> {
    let (Some(b), Some(g)) = (srgb(bg), srgb(ground)) else {
        return None;
    };
    // Neutral fills belong to `matched_ground`; this is only for the ones
    // it declined.
    let spread = b.0.max(b.1).max(b.2) - b.0.min(b.1).min(b.2);
    if spread <= NEUTRAL_SPREAD_MAX {
        return None;
    }
    let (bl, gl) = (luminance(b), luminance(g));
    let foreign = if gl >= 0.5 {
        bl <= FOREIGN_TINT_DARK_MAX_L
    } else {
        bl >= FOREIGN_TINT_LIGHT_MIN_L
    };
    if !foreign {
        return None;
    }
    let (h, sat, l) = to_hsl(b);
    Some(emit(from_hsl(h, sat, 1.0 - l), palette))
}

/// RGB to hue, saturation and lightness, each 0.0 to 1.0 (hue in turns).
fn to_hsl(c: (u8, u8, u8)) -> (f64, f64, f64) {
    let (r, g, b) = (
        f64::from(c.0) / 255.0,
        f64::from(c.1) / 255.0,
        f64::from(c.2) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d.abs() < f64::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if (max - r).abs() < f64::EPSILON {
        ((g - b) / d).rem_euclid(6.0)
    } else if (max - g).abs() < f64::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, s, l)
}

/// The inverse of [`to_hsl`].
fn from_hsl(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h * 6.0;
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hp as u8 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to8 = |v: f64| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    (to8(r), to8(g), to8(b))
}

/// Block elements, shades, braille, and the legacy-computing set: cells
/// whose colors are image pixels or plot points. "Text readability" has
/// no meaning there and recoloring corrupts the picture.
fn paints_pixels(ch: char) -> bool {
    matches!(u32::from(ch), 0x2580..=0x259F | 0x2800..=0x28FF | 0x1FB00..=0x1FBFF)
}

/// A computed color, in the terminal vocabulary the theme itself uses:
/// truecolor when the palette resolved truecolor, a 256-color grid entry
/// otherwise.
fn emit(rgb: (u8, u8, u8), palette: Option<&[RtColor; 16]>) -> RtColor {
    let truecolor = matches!(
        palette,
        Some(p) if p.iter().any(|c| matches!(c, RtColor::Rgb(..)))
    );
    if truecolor {
        RtColor::Rgb(rgb.0, rgb.1, rgb.2)
    } else {
        RtColor::Indexed(cyclops_theme::derive_c256(rgb))
    }
}

/// RGB of a ratatui color, via the standard xterm-256 table for indexed
/// entries. Host palettes can remap 0..15, but a clamp keyed on the
/// standard values is right far more often than no clamp at all.
fn srgb(c: RtColor) -> Option<(u8, u8, u8)> {
    match c {
        RtColor::Rgb(r, g, b) => Some((r, g, b)),
        RtColor::Indexed(i) => Some(xterm_rgb(i)),
        _ => None,
    }
}

fn xterm_rgb(i: u8) -> (u8, u8, u8) {
    const STD16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match i {
        0..=15 => STD16[usize::from(i)],
        16..=231 => {
            let i = i - 16;
            let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        232..=255 => {
            let g = 8 + 10 * (i - 232);
            (g, g, g)
        }
    }
}

/// WCAG 2.1 relative luminance, the same math the theme contrast tests
/// measure with (src/cyclops-theme/tests/shipped.rs).
fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn lerp(from: (u8, u8, u8), to: (u8, u8, u8), f: f64) -> (u8, u8, u8) {
    let mix = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * f).round() as u8;
    (mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

/// One frame of a fade between two chrome styles, `t` of the way from
/// `from` to `to`. The color half of `crate::animate`, which supplies `t`.
///
/// FOREGROUND ONLY. Background and every modifier come from `to` outright,
/// because an animation may move at most one side of a contrast pair: a
/// fill crossfade moves both, and its midpoint has no measured contrast
/// (panel ink on panel fading to panel ink on accent measures 1.69:1 at
/// t=0.5 in the shipped dark theme). A foreground fade against a fixed
/// ground travels between two pairs the theme already measured, which is
/// why the debug assert below holds every caller to a fixed ground.
///
/// Snaps to `to` when there is nothing to interpolate. Without truecolor an
/// interpolated color resolves to the nearest 256-cube entry, and the whole
/// dim-to-accent path collapses to four or five entries, so an eight-frame
/// fade shows four steps. Banding is worse than a snap.
///
/// The pane border is the caller that landed first (`canvas`, the focus
/// fade). The state cell, the notice and the sidebar row's status glyph are
/// still staged: `MotionFrame` already answers for them, and wiring each is
/// a call to this function at the point that picks its style.
pub(crate) fn blend(paint: &Paint, from: Style, to: Style, t: f32) -> Style {
    debug_assert!(
        from.bg == to.bg,
        "a fade moves the figure, never the ground"
    );
    if t <= 0.0 {
        return from;
    }
    if t >= 1.0 || !paint.truecolor {
        return to;
    }
    let (Some(a), Some(b)) = (from.fg.and_then(srgb), to.fg.and_then(srgb)) else {
        return to;
    };
    let (r, g, blue) = lerp(a, b, f64::from(t));
    to.fg(RtColor::Rgb(r, g, blue))
}

#[cfg(test)]
mod contrast_tests {
    use super::*;
    use crate::runtime::CellAttrs;

    fn cell(fg: Color, bg: Color) -> GridCell {
        GridCell {
            ch: 'x',
            zerowidth: Vec::new(),
            wide_spacer: false,
            attrs: CellAttrs {
                fg,
                bg,
                ..CellAttrs::default()
            },
        }
    }

    const WHITE: RtColor = RtColor::Rgb(254, 254, 254);
    const INK: RtColor = RtColor::Rgb(42, 42, 42);
    const PALETTE: [RtColor; 16] = [RtColor::Rgb(10, 10, 10); 16];

    /// The live symptom: an agent's pale gray, drawn for a dark
    /// terminal, must not vanish on paper.
    #[test]
    fn pale_gray_on_paper_darkens_to_readable() {
        let base = Style::new().fg(INK).bg(WHITE);
        let style = cell_style(
            &cell(Color::Indexed(250), Color::Default),
            base,
            Some(&PALETTE),
        );
        let fg = srgb(style.fg.unwrap()).unwrap();
        let bg = srgb(style.bg.unwrap()).unwrap();
        assert!(contrast(fg, bg) >= MIN_CONTRAST, "{fg:?} on {bg:?}");
        assert_ne!(
            style.fg.unwrap(),
            RtColor::Indexed(250),
            "it was 1.2:1 before"
        );
    }

    /// The other live symptom, with the stronger answer: an agent's
    /// neutral dark fill on a light theme is a ground painted for the
    /// wrong terminal. It becomes this theme's own panel (the measured
    /// codex composer fill, #393939), and the text on it reads.
    #[test]
    fn a_dark_fill_on_paper_becomes_the_themes_panel() {
        let base = Style::new().fg(INK).bg(WHITE);
        let style = cell_style(
            &cell(Color::Default, Color::Rgb(57, 57, 57)),
            base,
            Some(&PALETTE),
        );
        let bg = srgb(style.bg.unwrap()).unwrap();
        assert_eq!(bg, lerp((254, 254, 254), (42, 42, 42), PANEL_TINT));
        let fg = srgb(style.fg.unwrap()).unwrap();
        assert!(contrast(fg, bg) >= MIN_CONTRAST, "{fg:?} on {bg:?}");
    }

    /// A chromatic fill (a diff's green, a powerline segment) keeps its
    /// HUE, and only its lightness moves to this theme's side of the
    /// ground.
    ///
    /// The rule used to keep the color outright, on the reasoning that a
    /// chromatic fill is content and recoloring it destroys meaning. That
    /// reasoning is right and is why this does not re-ground the fill the
    /// way a neutral panel gets re-grounded: the hue IS the meaning, and a
    /// diff's red replaced by the theme's panel is a diff with the removed
    /// side erased.
    ///
    /// What the old rule missed is that lightness is not part of that
    /// meaning. A band painted at 5% lightness for a dark terminal stays a
    /// dark slab under a light theme, which is what the report was about.
    /// Mirroring lightness keeps green green and red red while putting
    /// both on the right side of the ground.
    #[test]
    fn a_chromatic_dark_fill_keeps_its_hue_and_flips_its_lightness() {
        let base = Style::new().fg(INK).bg(WHITE);
        let style = cell_style(
            &cell(Color::Default, Color::Rgb(14, 53, 18)),
            base,
            Some(&PALETTE),
        );
        let bg = srgb(style.bg.unwrap()).unwrap();
        assert_ne!(bg, (14, 53, 18), "a dark slab on a light theme has to move");
        assert!(
            luminance(bg) > luminance((14, 53, 18)),
            "and it moves toward the ground, not away: {bg:?}"
        );

        // Still green: the channel that led still leads, by the same
        // margin in hue terms.
        let (hue_before, sat_before, _) = to_hsl((14, 53, 18));
        let (hue_after, sat_after, _) = to_hsl(bg);
        assert!(
            (hue_before - hue_after).abs() < 0.02,
            "hue must survive: {hue_before} then {hue_after}"
        );
        assert!(
            (sat_before - sat_after).abs() < 0.02,
            "and so must saturation: {sat_before} then {sat_after}"
        );
        assert!(
            bg.1 > bg.0 && bg.1 > bg.2,
            "green is still the lead: {bg:?}"
        );

        // And the text on it still clears the floor.
        let fg = srgb(style.fg.unwrap()).unwrap();
        assert!(contrast(fg, bg) >= MIN_CONTRAST, "{fg:?} on {bg:?}");
    }

    /// A fill already on the theme's own side is native and untouched. The
    /// mirror is for foreign fills only, or every powerline segment an
    /// operator deliberately themed would get flipped.
    #[test]
    fn a_chromatic_fill_that_already_suits_the_ground_is_left_alone() {
        let base = Style::new().fg(INK).bg(WHITE);
        // A mid-light green: at home on a light ground.
        let native = Color::Rgb(180, 226, 185);
        let style = cell_style(&cell(Color::Default, native), base, Some(&PALETTE));
        assert_eq!(
            style.bg.unwrap(),
            RtColor::Rgb(180, 226, 185),
            "nothing foreign about it, so nothing to correct"
        );
    }

    /// The mirror: a near-white panel painted for a light terminal
    /// re-grounds on a dark theme, while a dark fill there is native
    /// and stays.
    #[test]
    fn a_light_fill_on_a_dark_ground_mirrors() {
        let base = Style::new().fg(WHITE).bg(INK);
        let style = cell_style(
            &cell(Color::Default, Color::Rgb(236, 236, 236)),
            base,
            Some(&PALETTE),
        );
        assert_eq!(
            srgb(style.bg.unwrap()).unwrap(),
            lerp((42, 42, 42), (254, 254, 254), PANEL_TINT)
        );
        let native = cell_style(
            &cell(Color::Default, Color::Rgb(57, 57, 57)),
            base,
            Some(&PALETTE),
        );
        assert_eq!(native.bg.unwrap(), RtColor::Rgb(57, 57, 57));
    }

    /// Reverse video is emphasis, not a mistaken ground: the pair
    /// passes through for the terminal to swap.
    #[test]
    fn reverse_video_keeps_its_colors() {
        let base = Style::new().fg(INK).bg(WHITE);
        let mut c = cell(Color::Default, Color::Rgb(57, 57, 57));
        c.attrs.reverse = true;
        let style = cell_style(&c, base, Some(&PALETTE));
        assert_eq!(style.bg.unwrap(), RtColor::Rgb(57, 57, 57));
        assert!(style.add_modifier.contains(Modifier::REVERSED));
    }

    /// Image pixels: a half-block's pair IS the picture. No re-ground,
    /// no floor, DIM forwarded as the modifier it always was.
    #[test]
    fn image_pixels_pass_through_untouched() {
        let base = Style::new().fg(INK).bg(WHITE);
        let mut px = cell(Color::Rgb(16, 16, 16), Color::Rgb(15, 15, 15));
        px.ch = '▄';
        px.attrs.dim = true;
        let style = cell_style(&px, base, Some(&PALETTE));
        assert_eq!(style.fg.unwrap(), RtColor::Rgb(16, 16, 16));
        assert_eq!(style.bg.unwrap(), RtColor::Rgb(15, 15, 15));
        assert!(style.add_modifier.contains(Modifier::DIM));
    }

    /// A pair already readable keeps its hue exactly.
    #[test]
    fn a_readable_color_is_untouched() {
        let base = Style::new().fg(INK).bg(WHITE);
        let style = cell_style(
            &cell(Color::Rgb(200, 0, 0), Color::Default),
            base,
            Some(&PALETTE),
        );
        assert_eq!(style.fg.unwrap(), RtColor::Rgb(200, 0, 0));
    }

    /// Rule 11: with color off (no palette) nothing is clamped, the
    /// program's own colors pass through untouched.
    #[test]
    fn no_palette_means_no_clamp() {
        let style = cell_style(
            &cell(Color::Indexed(250), Color::Indexed(255)),
            Style::new(),
            None,
        );
        assert_eq!(style.fg.unwrap(), RtColor::Indexed(250));
        assert_eq!(style.bg.unwrap(), RtColor::Indexed(255));
    }

    /// DIM folds into the clamp's own math: the fade happens here and
    /// the floor still holds, because the terminal's DIM would darken
    /// the color AFTER the clamp and un-read everything it fixed.
    #[test]
    fn dim_fades_in_the_math_and_still_clears_the_floor() {
        let base = Style::new().fg(INK).bg(WHITE);
        let mut dimmed = cell(Color::Default, Color::Rgb(30, 30, 30));
        dimmed.attrs.dim = true;
        let style = cell_style(&dimmed, base, Some(&PALETTE));
        assert!(
            !style.add_modifier.contains(Modifier::DIM),
            "colors on: DIM is consumed, never forwarded"
        );
        let fg = srgb(style.fg.unwrap()).unwrap();
        let bg = srgb(style.bg.unwrap()).unwrap();
        assert!(contrast(fg, bg) >= MIN_CONTRAST, "{fg:?} on {bg:?}");

        // Colors off: DIM passes through as the modifier it always was.
        let plain = cell_style(&dimmed, Style::new(), None);
        assert!(plain.add_modifier.contains(Modifier::DIM));
    }
}

/// Test fixtures shared by more than one surface's test module. Kept here,
/// rather than duplicated, because both `canvas` and `sidebar` (the two
/// glyph-stability tests) and both `canvas` and `tab_bar` (the two-pane
/// frame fixture) exercise the identical setup.
#[cfg(test)]
pub(crate) mod test_support {
    use ratatui::buffer::Buffer;

    use crate::layout::{parse_layout, resolve_layout};
    use crate::model::TabModel;
    use crate::theme::Paint;

    /// Two stacked panes whose tmux grid plus compact divider fills the
    /// 38x9 pane canvas used by the frame tests.
    pub(crate) fn two_pane_tab() -> TabModel {
        let node = parse_layout("4c3e,38x8,0,0[38x4,0,0,0,38x3,0,5,1]").unwrap();
        let layout = resolve_layout(&node, &[]).unwrap();
        TabModel {
            window_id: "@0".to_string(),
            name: "main".to_string(),
            layout,
            active_pane: "%0".to_string(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
        }
    }

    /// One pane filling the window: the case where a control that takes
    /// rows from a sibling has no sibling to take them from.
    pub(crate) fn single_pane_tab() -> TabModel {
        let node = parse_layout("b26f,38x8,0,0,0").unwrap();
        let layout = resolve_layout(&node, &[]).unwrap();
        TabModel {
            window_id: "@0".to_string(),
            name: "main".to_string(),
            layout,
            active_pane: "%0".to_string(),
            zoomed: false,
            minimized: std::collections::HashMap::new(),
        }
    }

    pub(crate) fn flatten(buf: &Buffer) -> String {
        (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect()
    }

    /// A theme deliberately unlike the default on every token the compact
    /// state cell can paint through — both `[state]` (idle/working/dead)
    /// and `[eye]` (the attention glyph) — so a color match against the
    /// default theme in a caller's test would mean its glyph check was
    /// vacuous. Shared by the two glyph-stability tests below.
    pub(crate) fn alt_test_theme_paint() -> Paint {
        let (theme, warnings) = cyclops_theme::Theme::parse(
            "name = \"alt-test\"\n\
             [state]\n\
             healthy = \"#123456\"\n\
             quiet = \"#234567\"\n\
             dead = \"#345678\"\n\
             [eye]\n\
             alert = \"#456789\"\n",
            "alt-test",
        )
        .expect("valid test theme");
        assert!(
            warnings.is_empty(),
            "unexpected theme warnings: {warnings:?}"
        );
        let mut paint = Paint::for_test();
        paint.theme = theme;
        paint
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How wide the conversation should be is the operator's call. The
    /// only bound left is the canvas beside the Messages pane, which is why
    /// a drag past the old forty-two column ceiling now widens the pane
    /// instead of stopping at a number chosen here.
    #[test]
    fn the_messages_pane_is_as_wide_as_the_operator_drags_it() {
        assert_eq!(clamp_messages_width(80, 200), 80);
        assert_eq!(clamp_messages_width(120, 200), 120);
        assert_eq!(
            clamp_messages_width(400, 200),
            200 - MAIN_MIN_WIDTH,
            "the canvas keeps its minimum, and nothing narrower bounds the Messages pane"
        );
        assert_eq!(clamp_messages_width(1, 200), MESSAGES_MIN_WIDTH);
        // A drag from a column near the left edge of a wide terminal is
        // the same request expressed as a position.
        assert_eq!(messages_width_for_column(60, 200), 140);
    }

    #[test]
    fn a_narrow_terminal_keeps_the_main_canvas_minimum() {
        for terminal_width in 21..=33 {
            let messages_pane = clamp_messages_width(u16::MAX, terminal_width);
            assert_eq!(
                terminal_width - messages_pane,
                MAIN_MIN_WIDTH,
                "terminal width {terminal_width} left only {} canvas columns",
                terminal_width - messages_pane
            );
        }
    }

    #[test]
    fn sidebar_resize_is_bounded_by_readability_and_half_the_terminal() {
        assert_eq!(clamp_sidebar_width(1, 200), SIDEBAR_MIN_WIDTH);
        assert_eq!(clamp_sidebar_width(100, 200), SIDEBAR_MAX_WIDTH);
        assert_eq!(clamp_sidebar_width(100, 50), 25);
        assert_eq!(sidebar_width_for_column(30, 50), 25);
        // Away from the clamp's edges, the column IS the width — no
        // plus-one. The handle sits on the border column the width already
        // treats as its own last-plus-one, so mapping it straight through
        // is what makes a drag begun on that border snap-free.
        assert_eq!(sidebar_width_for_column(30, 200), 30);
    }

    #[test]
    fn chrome_canvas_excludes_sidebar_and_tab_bar() {
        let areas = chrome_areas_for(
            Rect::new(0, 0, 200, 50),
            true,
            SIDEBAR_MIN_WIDTH,
            true,
            false,
            24,
        );
        assert_eq!(areas.sidebar, Some(Rect::new(0, 0, SIDEBAR_MIN_WIDTH, 50)));
        assert_eq!(areas.rail, None, "an open panel needs no rail");
        assert_eq!(
            areas.messages_rail,
            Some(Rect::new(199, 0, 1, 50)),
            "collapsed Messages pane leaves one rail column on the right"
        );
        assert_eq!(
            areas.tab_bar,
            Rect::new(SIDEBAR_MIN_WIDTH, 0, 200 - SIDEBAR_MIN_WIDTH - 1, 1)
        );
        assert_eq!(
            areas.canvas,
            Rect::new(SIDEBAR_MIN_WIDTH, 1, 200 - SIDEBAR_MIN_WIDTH - 1, 49)
        );
    }

    /// The collapsed shape: no panel rectangle, a one-column rail in its
    /// place, and every other column back to the canvas.
    #[test]
    fn a_collapsed_sidebar_leaves_one_rail_column_and_gives_back_the_rest() {
        let areas = chrome_areas_for(Rect::new(0, 0, 200, 50), false, 22, true, false, 24);
        assert_eq!(areas.sidebar, None);
        assert_eq!(areas.rail, Some(Rect::new(0, 0, 1, 50)));
        assert_eq!(areas.messages_rail, Some(Rect::new(199, 0, 1, 50)));
        assert_eq!(areas.tab_bar, Rect::new(1, 0, 198, 1));
        assert_eq!(areas.canvas, Rect::new(1, 1, 198, 49));
    }

    /// An open Messages pane carves its peer region from the right edge.
    #[test]
    fn an_open_messages_pane_carves_width_from_the_right() {
        let areas = chrome_areas_for(
            Rect::new(0, 0, 200, 50),
            false,
            22,
            true,
            true,
            MESSAGES_MIN_WIDTH,
        );
        assert_eq!(areas.sidebar, None);
        assert_eq!(areas.rail, Some(Rect::new(0, 0, 1, 50)));
        assert_eq!(
            areas.messages,
            Some(Rect::new(
                200 - MESSAGES_MIN_WIDTH,
                0,
                MESSAGES_MIN_WIDTH,
                50
            ))
        );
        assert_eq!(areas.messages_rail, None);
        assert_eq!(
            areas.tab_bar,
            Rect::new(1, 0, 200 - 1 - MESSAGES_MIN_WIDTH, 1)
        );
        assert_eq!(
            areas.canvas,
            Rect::new(1, 1, 200 - 1 - MESSAGES_MIN_WIDTH, 49)
        );
    }

    #[test]
    fn tmux_sizing_canvas_is_always_the_visible_agent_canvas() {
        for sidebar_visible in [false, true] {
            for tab_bar_visible in [false, true] {
                for terminal_width in 1..=200 {
                    let area = Rect::new(0, 0, terminal_width, 40);
                    let closed =
                        chrome_areas_for(area, sidebar_visible, 24, tab_bar_visible, false, 24);
                    let open = chrome_areas_for(
                        area,
                        sidebar_visible,
                        24,
                        tab_bar_visible,
                        true,
                        u16::MAX,
                    );
                    if let Some(messages) = open.messages {
                        if messages.width > MESSAGES_RAIL_WIDTH {
                            assert!(open.canvas.width < closed.canvas.width);
                        }
                    }
                    assert_eq!(
                        closed.tmux_sizing_canvas(),
                        closed.canvas,
                        "closed sizing must match its painted canvas"
                    );
                    assert_eq!(
                        open.tmux_sizing_canvas(),
                        open.canvas,
                        "open sizing must stop where the Messages pane begins"
                    );
                }
            }
        }
    }

    /// Hiding the strip is the operator's own choice now, not a tab count:
    /// no bar rectangle, and the canvas keeps the row the bar would have
    /// taken, whatever the workspace holds.
    #[test]
    fn a_hidden_tab_bar_gives_the_canvas_its_row() {
        let areas = chrome_areas_for(
            Rect::new(0, 0, 200, 50),
            true,
            SIDEBAR_MIN_WIDTH,
            false,
            false,
            24,
        );
        assert_eq!(areas.tab_bar.height, 0);
        assert_eq!(
            areas.canvas,
            Rect::new(SIDEBAR_MIN_WIDTH, 0, 200 - SIDEBAR_MIN_WIDTH - 1, 50)
        );
    }

    /// Both chrome edges gone at once: the rail keeps its column, the bar
    /// keeps none, and the canvas is exactly what is left.
    #[test]
    fn a_collapsed_rail_and_a_hidden_bar_compose() {
        let areas = chrome_areas_for(Rect::new(0, 0, 200, 50), false, 22, false, false, 24);
        assert_eq!(areas.rail, Some(Rect::new(0, 0, 1, 50)));
        assert_eq!(areas.messages_rail, Some(Rect::new(199, 0, 1, 50)));
        assert_eq!(areas.tab_bar.height, 0);
        assert_eq!(areas.canvas, Rect::new(1, 0, 198, 50));
    }

    /// `Paint::for_test` builds the 256-color path; the fade only runs on
    /// truecolor, so flip the one field that gates it.
    fn truecolor_paint() -> Paint {
        let mut paint = Paint::for_test();
        paint.truecolor = true;
        paint
    }

    /// The figure moves, the ground and the modifiers do not. A midpoint
    /// that is neither endpoint is the whole point; a midpoint that changed
    /// the background would be a contrast pair nobody measured.
    #[test]
    fn blend_moves_only_the_foreground() {
        let paint = truecolor_paint();
        let ground = RtColor::Rgb(26, 26, 26);
        let from = Style::new().fg(RtColor::Rgb(0, 0, 0)).bg(ground);
        let to = Style::new()
            .fg(RtColor::Rgb(100, 200, 40))
            .bg(ground)
            .add_modifier(Modifier::BOLD);

        assert_eq!(blend(&paint, from, to, 0.0), from, "t=0 is the start");
        assert_eq!(blend(&paint, from, to, 1.0), to, "t=1 is the target");

        let mid = blend(&paint, from, to, 0.5);
        assert_eq!(mid.fg, Some(RtColor::Rgb(50, 100, 20)));
        assert_eq!(mid.bg, Some(ground), "the ground never animates");
        assert!(
            mid.add_modifier.contains(Modifier::BOLD),
            "modifiers come from the target outright"
        );
    }

    /// Without truecolor every interpolated step would round to the same
    /// handful of 256-cube entries, so there is no fade to show and the
    /// target is what gets painted.
    #[test]
    fn blend_without_truecolor_snaps_to_the_target() {
        let paint = Paint::for_test();
        let from = Style::new().fg(RtColor::Rgb(0, 0, 0));
        let to = Style::new().fg(RtColor::Rgb(100, 200, 40));
        assert_eq!(blend(&paint, from, to, 0.5), to);

        // Same for a pair whose ink is not a color at all: with NO_COLOR
        // both endpoints are the empty style, and there is nothing to
        // interpolate between.
        let plain = Paint::without_color_for_test();
        assert_eq!(blend(&plain, Style::new(), Style::new(), 0.5), Style::new());
    }

    /// A glyph that cannot fit whole must not be written at all, and
    /// nothing may be written outside the bounds it was given. A wide
    /// glyph occupies two cells, so cutting one in half would either
    /// corrupt the neighbouring surface or leave a spacer cell that the
    /// next diff has no reason to repair. Combining marks ride the glyph
    /// they follow and must not be counted, or the budget drifts and the
    /// text runs long.
    #[test]
    fn a_clipped_wide_or_combining_glyph_never_writes_outside_its_bounds() {
        let area = Rect::new(0, 0, 12, 3);
        // The surface under test is the left half; the right half stands
        // in for whatever else owns those cells.
        let bounds = Rect::new(0, 1, 6, 1);
        for text in [
            "漢字漢字漢字漢字",
            "e\u{0301}e\u{0301}e\u{0301}e\u{0301}e\u{0301}e\u{0301}e\u{0301}",
            "a漢b字c漢d字",
        ] {
            let mut buf = Buffer::empty(area);
            overlay_text_ellipsized(&mut buf, bounds, bounds.x, bounds.y, text, Style::new());

            for x in bounds.x + bounds.width..area.width {
                assert_eq!(
                    buf.cell((x, bounds.y)).unwrap().symbol(),
                    " ",
                    "{text:?} wrote past its bounds at column {x}"
                );
            }
            for y in [0u16, 2] {
                for x in 0..area.width {
                    assert_eq!(
                        buf.cell((x, y)).unwrap().symbol(),
                        " ",
                        "{text:?} wrote into row {y}, which it was not given"
                    );
                }
            }
            // Truncation ends in the ellipsis rather than half a glyph.
            // Measuring the row's own width back would double-count the
            // spacer cell a wide glyph occupies, so the readable property
            // is that the cut is marked and every cell outside the bounds
            // is untouched, which the assertions above pin.
            let written: String = (bounds.x..bounds.x + bounds.width)
                .map(|x| buf.cell((x, bounds.y)).unwrap().symbol().to_string())
                .collect();
            assert!(
                written.contains('\u{2026}'),
                "{text:?} was cut without an ellipsis: {written:?}"
            );
        }
    }

    /// `overlay_text_ellipsized`'s own contract, apart from any caller: a
    /// fit paints exactly as given, an overflow keeps whole chars up to the
    /// budget and ends in `…`, a one-column budget is the ellipsis by
    /// itself, and a wide glyph that would not fit whole inside the budget
    /// is dropped whole rather than split into an unpaired half-cell.
    #[test]
    fn overlay_text_ellipsized_fits_unchanged_or_ends_in_an_ellipsis() {
        let read = |buf: &Buffer, w: u16| -> String {
            (0..w).map(|x| buf[(x, 0)].symbol().to_string()).collect()
        };

        let bounds = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(bounds);
        overlay_text_ellipsized(&mut buf, bounds, 0, 0, "hi", Style::new());
        assert_eq!(read(&buf, 5), "hi   ", "a fit paints unchanged");

        let mut buf = Buffer::empty(bounds);
        overlay_text_ellipsized(&mut buf, bounds, 0, 0, "hello world", Style::new());
        assert_eq!(read(&buf, 5), "hell…", "overflow ends in an ellipsis");

        let one = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(one);
        overlay_text_ellipsized(&mut buf, one, 0, 0, "hello", Style::new());
        assert_eq!(
            read(&buf, 1),
            "…",
            "a one-column budget is the ellipsis alone"
        );

        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        let empty_bounds = Rect::new(0, 0, 0, 1);
        overlay_text_ellipsized(&mut buf, empty_bounds, 0, 0, "hello", Style::new());
        assert_eq!(read(&buf, 1), " ", "no columns at all paints nothing");

        // "视" is two columns wide; a 3-column budget can hold "a" plus the
        // ellipsis but not "a视" plus one, so "视" must be dropped whole
        // rather than truncated into a single stray cell.
        let three = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(three);
        overlay_text_ellipsized(&mut buf, three, 0, 0, "a视z", Style::new());
        assert_eq!(
            read(&buf, 3),
            "a… ",
            "a wide glyph is dropped whole, never split"
        );
    }

    #[test]
    fn cancelling_a_sidebar_drag_restores_its_starting_width() {
        // The pointer's own column is the width now (no plus-one — see
        // `sidebar_width_for_column`), so a drag that started at column 27
        // restores to width 27, not 28.
        let mut drag = DragState::on_down(DragTarget::Sidebar, 27, 5);
        drag.on_move(38, 5);
        assert_eq!(sidebar_width_on_cancel(&drag, 100), Some(27));

        let tab = DragState::on_down(
            DragTarget::Tab {
                window_id: "@0".into(),
            },
            27,
            5,
        );
        assert_eq!(sidebar_width_on_cancel(&tab, 100), None);
    }
}
