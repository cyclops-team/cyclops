//! Pane canvas and frames: laying out and painting the active tab's pane
//! grid, borders, split controls, and per-pane title chrome.
//!
//! Owns the pane-to-screen geometry (`pane_canvas`, `tmux_client_size`,
//! `outer_frames`) and the per-cell paint pass from a [`PaneRuntime`]'s
//! visible cells into the Ratatui buffer. Does not own tmux commands,
//! persistence, or daemon queries — [`WindowPaintCtx`] carries everything
//! this needs to paint one frame, read-only.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};
use ratatui::text::Span;

use crate::copy;
use crate::decoration::DecorationSnapshot;
use crate::drag::{DragState, DragTarget};
use crate::input::mouse::{HitMap, HitTarget, PaneGeometry};
use crate::layout::{layout_gap_overhead, layout_geometry, DividerSeg, PaneGaps};
use crate::model::{PaneSlot, RuntimeRegistry, TabModel};
use crate::resilience::LinkState;
use crate::runtime::{CellPos, PaneRuntime};
use crate::selection::Selection;
use crate::theme::{self, Paint};

/// Chrome state passed through the window paint pass.
pub struct WindowPaintCtx<'a> {
    pub link: LinkState,
    pub paused: &'a std::collections::HashSet<String>,
    /// Panes collapsed to their title bar, mapped to the height each had
    /// before. Read here only to pick which chevron a pane's minimize
    /// control shows.
    pub minimized: &'a std::collections::HashMap<String, u16>,
    pub hits: &'a mut HitMap,
    pub decoration: &'a DecorationSnapshot,
    pub selection: Option<&'a Selection>,
    pub drag: Option<&'a DragState>,
    /// The workspace's transient notice, painted on the focused pane's
    /// bottom border. See [`paint_notice`] for why that row.
    pub notice: Option<&'a str>,
    /// The hardware cursor when the focused pane shows one.
    pub cursor: Option<HostCursor>,
    /// This frame's position in any fade the motion clock is running.
    /// [`crate::animate::MotionFrame::none`] when motion is off, which
    /// makes every read below snap straight to the destination style.
    pub motion: crate::animate::MotionFrame<'a>,
}

/// What the focused pane asks the host terminal's real cursor to do this
/// frame: where to sit, and which DECSCUSR shape to take. The workspace
/// never paints a cursor cell of its own — the one real cursor is the
/// only way a bar or underline can render at all in a cell grid — so the
/// pane's requested shape and blink pass through to the host instead of
/// being approximated with cell styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCursor {
    pub x: u16,
    pub y: u16,
    pub shape: crate::runtime::CursorShape,
    pub blink: bool,
}

/// One cell of breathing room around the pane canvas. Compact separator
/// bands preserve one border for each sibling pane.
pub const PANE_MARGIN: u16 = 1;

/// Compact pane separation: each sibling keeps its own border, with no extra
/// blank cell between them. Both split directions therefore use two cells,
/// one less than the previous border/blank/border band.
pub const PANE_GAPS: PaneGaps = PaneGaps {
    columns: 2,
    rows: 2,
};

/// The swap handle in every frame's bottom-right corner cell. Braille dots
/// read as grabbable texture the way GUI drag grips do, stay one narrow
/// cell in every monospace font, and belong to no box-drawing family, so
/// the calm border language survives with exactly one cell of handle
/// chrome. Painted with the same border tokens as the frame, focused and
/// unfocused alike.
pub const PANE_GRIP: &str = "[⠿]";

/// The minimize control, at the LEFT end of a pane's top border.
///
/// Chevrons rather than a `_`/`□` pair, because the rest of the chrome
/// already says "this opens" with `▾` and "this is shut" with `▸`. These
/// point the way the click moves the pane: `▴` collapses it up into its own
/// title bar, `▾` brings it back down.
///
/// Left, opposite the split and swap controls, because it acts on the whole
/// pane rather than on its edges, and because the right end is already
/// three controls deep.
pub const PANE_MINIMIZE: &str = "[▴]";
pub const PANE_RESTORE: &str = "[▾]";

/// Rows a minimized pane keeps. tmux clamps to its own floor, so asking for
/// one row gets whatever the smallest real pane is; the identity an
/// operator is looking for is painted on the border above it either way.
pub const MINIMIZED_ROWS: u16 = 1;

/// The six symbols one frame draws its border with.
///
/// Two sets exist so focus has a shape and not only a hue. A frame already
/// owns these cells, so the heavier set costs nothing to paint and is the
/// encoding that survives `NO_COLOR`, a screenshot, and a reader who cannot
/// separate two blues (rule 11). It also puts the loudest chrome on the
/// pane being worked in rather than on the sidebar beside it.
struct BorderGlyphs {
    horizontal: &'static str,
    vertical: &'static str,
    top_left: &'static str,
    top_right: &'static str,
    bottom_left: &'static str,
    bottom_right: &'static str,
}

/// A pane at rest: light lines (U+2500, U+2502) on rounded corners
/// (U+256D..U+2570), the calm default.
const BORDER_REST: BorderGlyphs = BorderGlyphs {
    horizontal: "─",
    vertical: "│",
    top_left: "╭",
    top_right: "╮",
    bottom_left: "╰",
    bottom_right: "╯",
};

/// The focused pane: the double set, U+2550..U+255D.
///
/// Double rather than heavy (U+2501, U+250F and neighbors) because the
/// double set came through CP437 into every terminal font, while a font
/// missing the heavy glyphs substitutes the light ones and erases the cue
/// in exactly the plain terminals it exists for. Both sets sit in the Box
/// Drawing block, so neither is wide and neither needs a spacer cell.
///
/// The bottom-right corner is a plain corner: the grip moved to the top
/// border beside the split controls. Kept in the set because
/// same as the set at rest.
const BORDER_FOCUSED: BorderGlyphs = BorderGlyphs {
    horizontal: "═",
    vertical: "║",
    top_left: "╔",
    top_right: "╗",
    bottom_left: "╚",
    bottom_right: "╝",
};

/// The rectangle occupied by pane content plus internal separators: the
/// canvas inset by [`PANE_MARGIN`] when there is room.
pub fn pane_canvas(canvas: Rect) -> Rect {
    if canvas.width > 2 * PANE_MARGIN && canvas.height > 2 * PANE_MARGIN {
        Rect::new(
            canvas.x + PANE_MARGIN,
            canvas.y + PANE_MARGIN,
            canvas.width - 2 * PANE_MARGIN,
            canvas.height - 2 * PANE_MARGIN,
        )
    } else {
        canvas
    }
}

/// Grid size declared to tmux for the active tab. Separator overhead is
/// removed here and restored only as chrome by [`paint_window`].
pub fn tmux_client_size(canvas: Rect, tab: &TabModel) -> (u16, u16) {
    let inner = pane_canvas(canvas);
    let (gap_width, gap_height) = if tab.zoomed {
        (0, 0)
    } else {
        layout_gap_overhead(&tab.layout, PANE_GAPS)
    };
    (
        inner.width.saturating_sub(gap_width),
        inner.height.saturating_sub(gap_height),
    )
}

/// Render every pane of the active window. The gaps tmux leaves between
/// panes — and the margin around them — paint as the chrome ground, so
/// panes read as cards with a gutter, not boxes sharing a line; the
/// focused pane gets an accent ring drawn in its surrounding gutter
/// cells.
pub fn paint_window(
    tab: &TabModel,
    runtimes: &RuntimeRegistry,
    canvas: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
) {
    if canvas.width == 0 || canvas.height == 0 {
        return;
    }
    // Ground the whole canvas: margins and pane gaps become gutter.
    buf.set_style(canvas, theme::chrome_panel(paint));
    let inner = pane_canvas(canvas);
    let (slots, dividers) = if tab.zoomed {
        (
            vec![PaneSlot {
                pane_id: tab.active_pane.clone(),
                rect: inner,
                focused: true,
            }],
            Vec::new(),
        )
    } else {
        let geometry = layout_geometry(&tab.layout, inner, &tab.active_pane, PANE_GAPS);
        (geometry.slots, geometry.dividers)
    };
    for slot in &slots {
        paint_pane_slot(slot, runtimes, buf, paint, ctx);
    }
    let frames = outer_frames(&slots);
    // Every pane owns a quiet boundary. Paint the focused pane last so its
    // accent wins where nested borders intersect.
    for (slot, frame) in slots.iter().zip(&frames).filter(|(slot, _)| !slot.focused) {
        paint_pane_frame(
            slot,
            has_vertical_neighbour(slot, &slots),
            *frame,
            canvas,
            buf,
            paint,
            ctx,
            scroll_depth(runtimes, slot),
        );
    }
    for (slot, frame) in slots.iter().zip(&frames).filter(|(slot, _)| slot.focused) {
        paint_pane_frame(
            slot,
            has_vertical_neighbour(slot, &slots),
            *frame,
            canvas,
            buf,
            paint,
            ctx,
            scroll_depth(runtimes, slot),
        );
    }
    // Shared pane borders are resize handles. Put divider regions above the
    // generic frame regions, then restore the visibly overlaid controls as
    // the most specific hit targets. The frame rects are not needed here
    // any more: all three controls, the grip included, are placed from the
    // slot's own rect by `pane_controls`.
    push_divider_hits(&dividers, ctx.hits);
    for slot in slots.iter() {
        push_pane_overlay_hits(
            slot,
            has_vertical_neighbour(slot, &slots),
            canvas,
            ctx.decoration,
            ctx.hits,
        );
    }
    if let Some(drag) = ctx.drag.filter(|d| d.is_active()) {
        paint_drag_preview(drag, buf, paint);
    }
}

/// The border box for each slot: its own rect, grown to the shared outer
/// edge on any axis where no sibling lies beyond it.
///
/// The pane area has to read as one rectangle, and without this it does
/// not. tmux gives every column the same height, but a column expands one
/// screen cell per split into two (`PANE_GAPS`), so a column carrying three
/// panes needs more chrome rows than a column carrying one.
/// [`layout_gap_overhead`] reserves that difference from the deepest branch,
/// which leaves every shallower branch finishing that many cells short and
/// the deepest one looking like it overhangs the rest.
///
/// Only the border moves. Leaf content rects stay exactly the size tmux
/// reported, because a live child terminal maps one runtime cell to one
/// screen cell and cropping or stretching it would corrupt what it drew —
/// the surplus simply becomes gutter inside the box, which is already how
/// this renderer treats every cell tmux does not own.
fn outer_frames(slots: &[PaneSlot]) -> Vec<Rect> {
    let edge_right = slots
        .iter()
        .map(|slot| slot.rect.x + slot.rect.width)
        .max()
        .unwrap_or(0);
    let edge_bottom = slots
        .iter()
        .map(|slot| slot.rect.y + slot.rect.height)
        .max()
        .unwrap_or(0);
    slots
        .iter()
        .map(|slot| {
            let own = slot.rect;
            let shares_rows =
                |other: &Rect| other.y < own.y + own.height && own.y < other.y + other.height;
            let shares_cols =
                |other: &Rect| other.x < own.x + own.width && own.x < other.x + other.width;
            let has_pane_below = slots
                .iter()
                .any(|o| o.rect.y >= own.y + own.height && shares_cols(&o.rect));
            let has_pane_beyond = slots
                .iter()
                .any(|o| o.rect.x >= own.x + own.width && shares_rows(&o.rect));
            Rect::new(
                own.x,
                own.y,
                if has_pane_beyond {
                    own.width
                } else {
                    edge_right.saturating_sub(own.x)
                },
                if has_pane_below {
                    own.height
                } else {
                    edge_bottom.saturating_sub(own.y)
                },
            )
        })
        .collect()
}

/// Divider gap cells stay grabbable for resize even though they paint as
/// plain gutter.
fn push_divider_hits(dividers: &[DividerSeg], hits: &mut HitMap) {
    for seg in dividers {
        hits.push(
            seg.rect,
            HitTarget::Divider {
                pane_id: seg.pane_id.clone(),
                dir: seg.dir,
            },
        );
    }
}

/// A pane border in the gutter cells around `rect`, clipped to `bounds`.
/// The margin and explicit separators guarantee those cells are chrome,
/// never another pane's content. `glyphs` says which set the frame draws
/// with: the caller picks it by focus, the same place it picks the style.
fn paint_pane_border(
    rect: Rect,
    bounds: Rect,
    buf: &mut Buffer,
    style: Style,
    glyphs: &BorderGlyphs,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let left = rect.x as i32 - 1;
    let top = rect.y as i32 - 1;
    let right = (rect.x + rect.width) as i32;
    let bottom = (rect.y + rect.height) as i32;
    let mut set = |x: i32, y: i32, sym: &str| {
        if x < bounds.x as i32
            || x >= (bounds.x + bounds.width) as i32
            || y < bounds.y as i32
            || y >= (bounds.y + bounds.height) as i32
        {
            return;
        }
        if let Some(cell) = buf.cell_mut((x as u16, y as u16)) {
            cell.set_symbol(sym);
            cell.set_style(style);
        }
    };
    for x in rect.x as i32..right {
        set(x, top, glyphs.horizontal);
        set(x, bottom, glyphs.horizontal);
    }
    for y in rect.y as i32..bottom {
        set(left, y, glyphs.vertical);
        set(right, y, glyphs.vertical);
    }
    set(left, top, glyphs.top_left);
    set(right, top, glyphs.top_right);
    set(left, bottom, glyphs.bottom_left);
    set(right, bottom, glyphs.bottom_right);
}

fn paint_pane_slot(
    slot: &PaneSlot,
    runtimes: &RuntimeRegistry,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
) {
    let vis = slot.rect;
    if vis.width == 0 || vis.height == 0 {
        return;
    }
    let mut base = theme::pane_cell(paint);
    if matches!(ctx.link, LinkState::Reconnecting { .. }) {
        base = base.add_modifier(Modifier::DIM);
    }
    if let Some(runtime) = runtimes.get(&slot.pane_id) {
        let selection = ctx.selection.filter(|s| s.pane_id == slot.pane_id);
        paint_pane_cells(
            runtime,
            selection,
            vis,
            buf,
            base,
            theme::selection_highlight(paint),
            paint.pane_palette().as_ref(),
        );
        if slot.focused {
            let cur = runtime.cursor();
            if cur.visible && runtime.at_tail() && cur.col < vis.width && cur.row < vis.height {
                ctx.cursor = Some(HostCursor {
                    x: vis.x + cur.col,
                    y: vis.y + cur.row,
                    shape: cur.shape,
                    blink: cur.blink,
                });
            }
        }
    } else {
        fill_blank(vis, buf, base);
    }

    ctx.hits.push(
        vis,
        HitTarget::PaneBody {
            pane_id: slot.pane_id.clone(),
        },
    );
    ctx.hits.push_geometry(PaneGeometry {
        pane_id: slot.pane_id.clone(),
        inner: vis,
    });

    // Transient link state, top-left overlay.
    let note = if matches!(ctx.link, LinkState::Reconnecting { .. }) {
        Some(copy::RECONNECTING_NOTE)
    } else if ctx.paused.contains(&slot.pane_id) {
        Some(copy::PAUSED_NOTE)
    } else {
        None
    };
    if let Some(note) = note {
        super::overlay_text(
            buf,
            vis,
            vis.x,
            vis.y,
            &format!(" {note} "),
            theme::pane_border_focused(paint).add_modifier(Modifier::DIM),
        );
    }
}

/// Lines a pane's viewport sits back from its live tail; 0 when the pane
/// has no runtime yet.
fn scroll_depth(runtimes: &RuntimeRegistry, slot: &PaneSlot) -> usize {
    runtimes
        .get(&slot.pane_id)
        .map_or(0, PaneRuntime::scrolled_back)
}

/// Paint one pane's border, its corner swap grip, optional named-agent
/// chrome, and split controls. Unnamed panes stay textually quiet; their
/// muted boundary still makes the layout legible. `scrolled` is the pane's
/// scrollback depth; nonzero paints a dim hint on the top border.
fn paint_pane_frame(
    slot: &PaneSlot,
    can_shrink: bool,
    frame: Rect,
    bounds: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
    scrolled: usize,
) {
    // The frame, not the content rect: a box grown out to the shared edge
    // must stay grabbable over the whole boundary it actually draws.
    let vis = frame;
    // Focus moves both encodings at once, and this is the only place that
    // decides either: the accent color, and the heavier glyph set that
    // keeps focus readable when the color is gone.
    //
    // The glyph set flips at once and the color crosses over time. That
    // split is deliberate: the heavier glyphs are the encoding that has to
    // survive NO_COLOR and a screenshot, so they must never be caught
    // mid-way, while the accent is the part an eye can follow moving. With
    // motion off `focus` returns the endpoint and the blend is a snap.
    let border_glyphs = if slot.focused {
        &BORDER_FOCUSED
    } else {
        &BORDER_REST
    };
    // The endpoints never swap. `focus` answers how much accent this border
    // carries right now, 0.0 at rest and 1.0 focused, so the blend always
    // runs rest to focused and only `t` moves. Flipping the ends by focus
    // state applies the fade twice and lands a resting pane on the wrong
    // one.
    let border_style = super::blend(
        paint,
        theme::pane_border(paint),
        theme::pane_border_focused(paint),
        ctx.motion.focus(&slot.pane_id, slot.focused),
    );
    paint_pane_border(vis, bounds, buf, border_style, border_glyphs);
    // The bottom-right corner becomes the grip. Painted here, per frame,
    // right after this frame's own border: the focused frame repaints last, so
    // a shared pass would let its accent ring overwrite another pane's
    // grip where borders intersect.
    if slot.focused {
        if let Some(notice) = ctx.notice {
            paint_notice(vis, bounds, notice, buf, paint);
        }
    }

    let left = vis.x.saturating_sub(1).max(bounds.x);
    let top = vis.y.saturating_sub(1).max(bounds.y);
    let right = (vis.x + vis.width).min(bounds.x + bounds.width - 1);
    let bottom = (vis.y + vis.height).min(bounds.y + bounds.height - 1);
    ctx.hits.push(
        Rect::new(left, top, right.saturating_sub(left).saturating_add(1), 1),
        HitTarget::PaneFrame {
            pane_id: slot.pane_id.clone(),
        },
    );
    ctx.hits.push(
        Rect::new(
            left,
            bottom,
            right.saturating_sub(left).saturating_add(1),
            1,
        ),
        HitTarget::PaneFrame {
            pane_id: slot.pane_id.clone(),
        },
    );
    ctx.hits.push(
        Rect::new(left, top, 1, bottom.saturating_sub(top).saturating_add(1)),
        HitTarget::PaneFrame {
            pane_id: slot.pane_id.clone(),
        },
    );
    ctx.hits.push(
        Rect::new(right, top, 1, bottom.saturating_sub(top).saturating_add(1)),
        HitTarget::PaneFrame {
            pane_id: slot.pane_id.clone(),
        },
    );

    // The minimize control, at the left end of the top border.
    //
    // Only when the pane can actually shrink. A pane spanning the full
    // canvas height is the only pane in its column, and `resize-pane -y`
    // has nothing to take from it, so the control would sit there taking
    // clicks and doing nothing. That is the failure mode this chrome
    // language exists to avoid.
    let minimized = ctx.minimized.contains_key(&slot.pane_id);
    if let Some(cell) = minimize_cell(slot, bounds, can_shrink) {
        let glyph = if minimized {
            PANE_RESTORE
        } else {
            PANE_MINIMIZE
        };
        super::overlay_text(buf, bounds, cell.x, cell.y, glyph, border_style);
    }

    // Controls live in the border instead of overwriting the first row of
    // the child TUI. They remain available on unfocused panes.
    let controls = pane_controls(slot, bounds);
    let control_left = controls.map_or(right, |controls| controls.grip.x);
    if let Some(controls) = controls {
        super::overlay_text(
            buf,
            bounds,
            controls.grip.x,
            controls.grip.y,
            PANE_GRIP,
            border_style,
        );
        super::overlay_text(
            buf,
            bounds,
            controls.split_right.x,
            controls.split_right.y,
            "[|]",
            border_style,
        );
        super::overlay_text(
            buf,
            bounds,
            controls.split_down.x,
            controls.split_down.y,
            "[-]",
            border_style,
        );
    }

    // A scrolled pane says how deep it sits, dim on the top border flush
    // against the split controls. The title budget below stops at the
    // returned boundary, so the hint never collides with the title text,
    // and the frame's drag hit regions are untouched.
    let control_left = paint_scroll_hint(slot, bounds, control_left, scrolled, buf, border_style);

    let Some(decoration) = ctx
        .decoration
        .pane(&slot.pane_id)
        .filter(|decoration| decoration.label.is_some() || decoration.manifest.is_some())
    else {
        return;
    };
    let label = DecorationSnapshot::sidebar_name(decoration);
    let Some(title_bounds) = pane_title_rect(slot, bounds, control_left) else {
        return;
    };
    let status = DecorationSnapshot::primary_status(decoration);
    let shown_state = status.map(|status| {
        let full = format!("{} {}", status.glyph, status.word);
        // Four, not three: " · " leads the state and a single space
        // follows it. Without the trailing one the border rule resumes
        // against the glyph and `○────` reads as one joined mark rather
        // than a status next to a line.
        let full_suffix = 4usize.saturating_add(Span::raw(full.as_str()).width());
        if slot.focused
            && Span::raw(label).width().saturating_add(full_suffix)
                <= usize::from(title_bounds.width)
        {
            full
        } else {
            status.glyph.to_string()
        }
    });
    let suffix_width = shown_state
        .as_ref()
        .map(|state| {
            4u16.saturating_add(u16::try_from(Span::raw(state).width()).unwrap_or(u16::MAX))
        })
        .unwrap_or(0);
    let label_width = u16::try_from(Span::raw(label).width()).unwrap_or(u16::MAX);
    let label_budget = if suffix_width > 0 && title_bounds.width > suffix_width {
        label_width.min(title_bounds.width - suffix_width)
    } else {
        label_width.min(title_bounds.width)
    };
    let label_style = if decoration.label.is_some() {
        paint.role(label).add_modifier(Modifier::BOLD)
    } else {
        border_style.add_modifier(Modifier::BOLD)
    };
    let label_bounds = Rect::new(title_bounds.x, top, label_budget, 1);
    super::overlay_text(buf, label_bounds, label_bounds.x, top, label, label_style);
    let Some(shown_state) = shown_state.filter(|_| title_bounds.width > suffix_width) else {
        return;
    };
    let mut x = title_bounds.x.saturating_add(label_budget);
    super::overlay_text(buf, title_bounds, x, top, " · ", border_style);
    x = x.saturating_add(3);
    super::overlay_text(
        buf,
        title_bounds,
        x,
        top,
        &shown_state,
        if shown_state.starts_with('⚠') {
            theme::attention_eye(paint)
        } else {
            paint.state(
                status
                    .map(|status| status.color_state)
                    .unwrap_or(decoration.state),
            )
        },
    );
    // Close the label the way it opened, so the border rule restarts a cell
    // clear of the state instead of touching it. Budgeted for above.
    let state_width = u16::try_from(Span::raw(&shown_state).width()).unwrap_or(u16::MAX);
    super::overlay_text(
        buf,
        title_bounds,
        x.saturating_add(state_width),
        top,
        " ",
        border_style,
    );
}

/// Paint the workspace's transient notice along the focused frame's bottom
/// border.
///
/// That row and no other. It is chrome the workspace draws itself, so a
/// notice never covers a cell tmux owns and never moves one: nothing
/// resizes when a message appears or expires, which matters because a
/// reflow here would reflow every agent's TUI. It is also the border of
/// the pane the operator was just working in, so the confirmation lands
/// where the eye already is instead of at a screen edge. The top border is
/// spoken for by the pane's identity, state, and split controls, so the
/// bottom is the free one; it does carry the corner grip, which the text
/// stops short of, and under a stacked sibling it doubles as that pair's
/// resize band. Painting over a band is safe where painting over a
/// control would not be: the notice registers no hit region, so every
/// cell it tints still drags, and it is gone in a second either way.
///
/// Too narrow to hold the whole phrase means nothing is painted: half a
/// sentence on a border reads as corruption, not as feedback.
fn paint_notice(frame: Rect, bounds: Rect, notice: &str, buf: &mut Buffer, paint: &Paint) {
    let text = format!(" {notice} ");
    let width = u16::try_from(Span::raw(text.as_str()).width()).unwrap_or(u16::MAX);
    let y = frame.y.saturating_add(frame.height);
    if width > frame.width || y >= bounds.y.saturating_add(bounds.height) {
        return;
    }
    super::overlay_text(buf, bounds, frame.x, y, &text, theme::chrome_notice(paint));
}

fn pane_title_rect(slot: &PaneSlot, bounds: Rect, control_left: u16) -> Option<Rect> {
    let top = slot.rect.y.saturating_sub(1).max(bounds.y);
    let title_left = slot.rect.x.saturating_add(1);
    (title_left < control_left).then(|| Rect::new(title_left, top, control_left - title_left, 1))
}

/// Paint the scrollback depth hint ("12 back") right-aligned against
/// `control_left` on the top border. Returns the new left boundary for the
/// title strip: the hint's own start while it shows, `control_left`
/// untouched when the pane is at the tail or the border is too narrow.
fn paint_scroll_hint(
    slot: &PaneSlot,
    bounds: Rect,
    control_left: u16,
    scrolled: usize,
    buf: &mut Buffer,
    border_style: Style,
) -> u16 {
    if scrolled == 0 {
        return control_left;
    }
    let text = format!(" {scrolled} back ");
    let width = u16::try_from(text.chars().count()).unwrap_or(u16::MAX);
    let title_left = slot.rect.x.saturating_add(1);
    let Some(x) = control_left.checked_sub(width).filter(|x| *x > title_left) else {
        return control_left;
    };
    let top = slot.rect.y.saturating_sub(1).max(bounds.y);
    super::overlay_text(
        buf,
        bounds,
        x,
        top,
        &text,
        border_style.add_modifier(Modifier::DIM),
    );
    x
}

#[derive(Debug, Clone, Copy)]
struct PaneControls {
    grip: Rect,
    split_right: Rect,
    split_down: Rect,
}

/// Whether `slot` has a pane stacked above or below it.
///
/// Sharing columns is what makes two panes vertical neighbours, and a
/// vertical neighbour is the only thing that can absorb the rows a
/// minimize gives up. Two panes side by side both span the window's full
/// height, so neither can shrink for the other.
fn has_vertical_neighbour(slot: &PaneSlot, slots: &[PaneSlot]) -> bool {
    slots.iter().any(|other| {
        other.pane_id != slot.pane_id
            && other.rect.x < slot.rect.x + slot.rect.width
            && slot.rect.x < other.rect.x + other.rect.width
    })
}

/// Where a pane's minimize control paints, or `None` when the pane cannot
/// be minimized.
///
/// Two reasons it can be `None`. A pane with nothing stacked above or below
/// it has nowhere to put the rows it would give up, and `resize-pane -y`
/// simply will not move it; offering the control there is offering a click
/// that does nothing. And a pane too narrow to hold both this and the
/// right-hand trio without them touching keeps the trio, which is the older
/// and more used set.
fn minimize_cell(slot: &PaneSlot, bounds: Rect, can_shrink: bool) -> Option<Rect> {
    if !can_shrink {
        return None;
    }
    let vis = slot.rect;
    let left = vis.x.saturating_sub(1).max(bounds.x);
    let top = vis.y.saturating_sub(1).max(bounds.y);
    let right = (vis.x + vis.width).min(bounds.x + bounds.width - 1);
    // Three cells for this, three each for the trio, and two of border to
    // keep the two groups from meeting in the middle.
    if right.saturating_sub(left) < 14 {
        return None;
    }
    Some(Rect::new(left + 1, top, 3, 1))
}

fn pane_controls(slot: &PaneSlot, bounds: Rect) -> Option<PaneControls> {
    let vis = slot.rect;
    let left = vis.x.saturating_sub(1).max(bounds.x);
    let top = vis.y.saturating_sub(1).max(bounds.y);
    let right = (vis.x + vis.width).min(bounds.x + bounds.width - 1);
    // Three controls of three cells, plus two of border either side of
    // them. Under this a pane paints no controls at all rather than a
    // partial set, which is the rule the two-control version already had.
    if right.saturating_sub(left) < 11 {
        return None;
    }

    let split_down = Rect::new(right.saturating_sub(3), top, 3, 1);
    let split_right = Rect::new(split_down.x.saturating_sub(3), top, 3, 1);
    let grip = Rect::new(split_right.x.saturating_sub(3), top, 3, 1);
    Some(PaneControls {
        grip,
        split_right,
        split_down,
    })
}

/// Hit regions for the controls painted over a pane's frame: the labeled
/// title strip (a focus click, or the attention eye), the split buttons,
/// and the corner swap grip. Pushed after the divider bands so these
/// visibly overlaid cells win the hit test; the grip is the only one of
/// them that starts a drag.
fn push_pane_overlay_hits(
    slot: &PaneSlot,
    can_shrink: bool,
    bounds: Rect,
    decoration: &DecorationSnapshot,
    hits: &mut HitMap,
) {
    let right = (slot.rect.x + slot.rect.width).min(bounds.x + bounds.width - 1);
    let controls = pane_controls(slot, bounds);
    let control_left = controls.map_or(right, |controls| controls.grip.x);
    if let Some((pane, rect)) = decoration
        .pane(&slot.pane_id)
        .filter(|pane| pane.label.is_some())
        .zip(pane_title_rect(slot, bounds, control_left))
    {
        let target = if pane.needs_attention {
            HitTarget::AttentionIndicator {
                pane_id: slot.pane_id.clone(),
            }
        } else {
            HitTarget::PaneFrame {
                pane_id: slot.pane_id.clone(),
            }
        };
        hits.push(rect, target);
    }
    if let Some(controls) = controls {
        hits.push(
            controls.split_right,
            HitTarget::PaneSplitRight {
                pane_id: slot.pane_id.clone(),
            },
        );
        hits.push(
            controls.split_down,
            HitTarget::PaneSplitDown {
                pane_id: slot.pane_id.clone(),
            },
        );
        // The grip's hit rides with the other two now. It used to sit on
        // the opposite corner of the frame, one cell wide.
        hits.push(
            controls.grip,
            HitTarget::PaneGrip {
                pane_id: slot.pane_id.clone(),
            },
        );
    }
    if let Some(cell) = minimize_cell(slot, bounds, can_shrink) {
        hits.push(
            cell,
            HitTarget::PaneMinimize {
                pane_id: slot.pane_id.clone(),
            },
        );
    }
}

/// One pass from engine cells to buffer cells: the R1 design. There is no
/// intermediate full-grid copy, and the selection highlight is decided per
/// cell in the same visit instead of a second walk over a mirrored grid.
fn paint_pane_cells(
    runtime: &PaneRuntime,
    selection: Option<&Selection>,
    area: Rect,
    buf: &mut Buffer,
    base: Style,
    highlight: Style,
    palette: Option<&[RtColor; 16]>,
) {
    let range = selection.map(|sel| {
        if sel.from.row < sel.to.row || (sel.from.row == sel.to.row && sel.from.col <= sel.to.col) {
            (sel.from, sel.to)
        } else {
            (sel.to, sel.from)
        }
    });
    runtime.for_each_visible_cell(|col, row, cell| {
        if col >= area.width || row >= area.height {
            return;
        }
        let style = match range {
            Some((from, to)) if in_selection(col, row, from, to) => highlight,
            _ => super::cell_style(&cell, base, palette),
        };
        let Some(dst) = buf.cell_mut((area.x + col, area.y + row)) else {
            return;
        };
        if cell.wide_spacer || cell.ch == '\0' {
            dst.set_char(' ');
        } else if cell.zerowidth.is_empty() {
            dst.set_char(cell.ch);
        } else {
            // A combining mark or variation selector shares the base
            // char's column (`GridCell::zerowidth`); `set_char` only takes
            // one scalar, so the full grapheme needs `set_symbol`.
            let mut grapheme = String::from(cell.ch);
            grapheme.extend(cell.zerowidth.iter());
            dst.set_symbol(&grapheme);
        }
        dst.set_style(style);
    });
    // The grid can be smaller than the slot during a resize transient, and
    // every slot cell must repaint over the previous frame.
    let (gcols, grows) = runtime.size();
    for row in 0..area.height {
        for col in 0..area.width {
            if col >= gcols || row >= grows {
                if let Some(dst) = buf.cell_mut((area.x + col, area.y + row)) {
                    dst.set_char(' ');
                    dst.set_style(base);
                }
            }
        }
    }
}

/// Whether one cell sits inside a normalized selection range. Middle rows
/// select edge to edge; the end rows clip at the anchor columns.
fn in_selection(col: u16, row: u16, from: CellPos, to: CellPos) -> bool {
    (row > from.row || (row == from.row && col >= from.col))
        && (row < to.row || (row == to.row && col <= to.col))
}

fn fill_blank(area: Rect, buf: &mut Buffer, base: Style) {
    for row in 0..area.height {
        for col in 0..area.width {
            if let Some(dst) = buf.cell_mut((area.x + col, area.y + row)) {
                dst.set_char(' ');
                dst.set_style(base);
            }
        }
    }
}

fn paint_drag_preview(drag: &DragState, buf: &mut Buffer, paint: &Paint) {
    let style = theme::pane_border_focused(paint);
    let (x, y) = drag.current;
    let hint = match &drag.target {
        DragTarget::Divider { .. } => "↔",
        DragTarget::Pane { .. } | DragTarget::Tab { .. } => "⇄",
        DragTarget::Workspace { .. } | DragTarget::Agent { .. } => "⇅",
        DragTarget::Sidebar => "↔",
        // Vertical only: the sidebar's two panels trade rows, not columns.
        DragTarget::SidebarSplit => "↕",
        // Free in both axes, unlike every other drag here.
        DragTarget::Dialog => "✥",
    };
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(hint);
        cell.set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::style::Color as RtColor;
    use ratatui::Terminal;

    use super::*;
    use crate::decoration::DecorationSnapshot;
    use crate::layout::{parse_layout, resolve_layout};
    use crate::render::paint_tab_bar;
    use crate::render::test_support::{
        alt_test_theme_paint, flatten, single_pane_tab, two_pane_tab,
    };

    static EMPTY_MINIMIZED: std::sync::LazyLock<std::collections::HashMap<String, u16>> =
        std::sync::LazyLock::new(std::collections::HashMap::new);

    fn ctx_defaults<'a>(
        hits: &'a mut HitMap,
        paused: &'a std::collections::HashSet<String>,
        decoration: &'a DecorationSnapshot,
    ) -> WindowPaintCtx<'a> {
        WindowPaintCtx {
            link: LinkState::Live,
            paused,
            hits,
            decoration,
            selection: None,
            drag: None,
            notice: None,
            minimized: &EMPTY_MINIMIZED,
            cursor: None,
            // No clock: every fade reads as its endpoint, which is what
            // these tests assert about. The fade itself is covered by
            // `animate`'s own tests and by the focus-fade test below.
            motion: crate::animate::MotionFrame::none(),
        }
    }

    fn frame_slot(pane_id: &str, rect: Rect) -> PaneSlot {
        PaneSlot {
            pane_id: pane_id.into(),
            rect,
            focused: false,
        }
    }

    #[test]
    fn pane_cells_paint_on_test_backend() {
        let mut rt = crate::runtime::PaneRuntime::new(5, 2);
        rt.feed(b"X");
        let backend = TestBackend::new(5, 2);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            paint_pane_cells(
                &rt,
                None,
                f.area(),
                f.buffer_mut(),
                theme::pane_cell(&theme),
                theme::selection_highlight(&theme),
                theme.pane_palette().as_ref(),
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        assert_eq!(buf[(0, 0)].symbol(), "X");
        assert_eq!(buf[(1, 0)].symbol(), " ", "blank cells repaint as spaces");
    }

    // ------------------------------------------------- buffer-boundary fidelity
    //
    // `corpus.rs` and `fidelity.rs` prove the engine-to-`GridCell` bridge
    // keeps what the engine parsed. These feed the same bytes through a
    // real `PaneRuntime` and `paint_pane_cells` — the actual render path —
    // and read the Ratatui buffer a frame is drawn from, because a fix in
    // `GridCell` that never reaches `Buffer::cell.symbol()` fixes nothing a
    // user sees.

    fn paint_bytes(bytes: &[u8], cols: u16, rows: u16) -> Buffer {
        let mut rt = crate::runtime::PaneRuntime::new(cols, rows);
        rt.feed(bytes);
        let backend = TestBackend::new(cols, rows);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            paint_pane_cells(
                &rt,
                None,
                f.area(),
                f.buffer_mut(),
                theme::pane_cell(&theme),
                theme::selection_highlight(&theme),
                theme.pane_palette().as_ref(),
            );
        })
        .unwrap();
        term.backend().buffer().clone()
    }

    #[test]
    fn a_combining_mark_reaches_the_buffer_with_its_base_character() {
        let buf = paint_bytes("e\u{301}x".as_bytes(), 6, 1);
        assert_eq!(
            buf[(0, 0)].symbol(),
            "e\u{301}",
            "the accent must paint as part of e's cell instead of vanishing"
        );
        assert_eq!(buf[(1, 0)].symbol(), "x");
    }

    #[test]
    fn a_variation_selector_stays_in_the_symbol_and_the_glyph_stays_narrow() {
        let buf = paint_bytes("⚠\u{fe0f}x".as_bytes(), 6, 1);
        assert_eq!(buf[(0, 0)].symbol(), "⚠\u{fe0f}");
        assert_eq!(
            buf[(1, 0)].symbol(),
            "x",
            "VS16 must not widen the column the engine sized narrow \
             (fidelity.rs: a_variation_selector_does_not_widen_a_narrow_glyph)"
        );
    }

    #[test]
    fn a_wide_emoji_paints_a_spacer_cell_after_it() {
        let buf = paint_bytes("😀x".as_bytes(), 6, 1);
        assert_eq!(buf[(0, 0)].symbol(), "😀");
        assert_eq!(buf[(1, 0)].symbol(), " ", "the spacer column stays blank");
        assert_eq!(buf[(2, 0)].symbol(), "x");
    }

    #[test]
    fn every_underline_variant_reaches_the_buffer_as_the_one_modifier_ratatui_has() {
        // Ratatui's Modifier cannot say double/curl/dotted/dashed, so every
        // engine style narrows to UNDERLINED at this boundary; the five
        // styles are still told apart in GridCell (grid.rs's `Underline`
        // doc) and proven not to flatten early by fidelity.rs's
        // `every_underline_style_keeps_its_own_identity`.
        for bytes in [
            b"\x1b[4mU\x1b[0m".as_slice(),
            b"\x1b[4:2mU\x1b[0m".as_slice(),
            b"\x1b[4:3mU\x1b[0m".as_slice(),
            b"\x1b[4:4mU\x1b[0m".as_slice(),
            b"\x1b[4:5mU\x1b[0m".as_slice(),
        ] {
            let buf = paint_bytes(bytes, 4, 1);
            assert!(
                buf[(0, 0)].modifier.contains(Modifier::UNDERLINED),
                "{bytes:?} must reach the buffer underlined"
            );
        }
    }

    #[test]
    fn hidden_and_strikeout_reach_the_buffer_as_their_own_modifiers() {
        let buf = paint_bytes(b"\x1b[8mH\x1b[0m", 4, 1);
        assert!(buf[(0, 0)].modifier.contains(Modifier::HIDDEN));

        let buf = paint_bytes(b"\x1b[9mS\x1b[0m", 4, 1);
        assert!(buf[(0, 0)].modifier.contains(Modifier::CROSSED_OUT));
    }

    // ------------------------------------------------- themed pane ground
    //
    // The pane-body color contract (docs/guides/themes.md): with colors on
    // the body owns its ground (surface.fg on surface.bg) and maps ANSI
    // 0..15 through the theme's palette; with NO_COLOR nothing themed is
    // left behind and the program's own colors pass through untouched.

    /// One row exercising every color family the contract covers: unstyled
    /// text, ANSI-16 fg (SGR 31/91), 256-color fg (38;5;196), reverse
    /// video, and an ANSI-16 bg (SGR 41). Shared by the colors-on and
    /// NO_COLOR probes so both read the identical content.
    fn ground_probe_buffer(paint: &Paint) -> Buffer {
        let mut rt = crate::runtime::PaneRuntime::new(12, 2);
        rt.feed(b"ab \x1b[31mR\x1b[0m \x1b[91mB\x1b[0m \x1b[38;5;196mX\x1b[0m \x1b[7mV\x1b[0m \x1b[41mQ\x1b[0m");
        let backend = TestBackend::new(12, 2);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            paint_pane_cells(
                &rt,
                None,
                f.area(),
                f.buffer_mut(),
                theme::pane_cell(paint),
                theme::selection_highlight(paint),
                paint.pane_palette().as_ref(),
            );
        })
        .unwrap();
        term.backend().buffer().clone()
    }

    #[test]
    fn pane_content_owns_its_ground_and_maps_ansi16_through_the_theme() {
        // Truecolor makes the mapping observable: shipped palette
        // fallbacks are the literal ANSI index by design (tokens::PALETTE),
        // so in 256-color mode themed Indexed(1) and host Indexed(1)
        // compare equal and the assertions below would be vacuous.
        let mut paint = Paint::for_test();
        paint.truecolor = true;
        let buf = ground_probe_buffer(&paint);
        let ground = theme::pane_cell(&paint);
        let (fg, bg) = (ground.fg.unwrap(), ground.bg.unwrap());

        // The body owns its ground: no cell lets the host show through.
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                assert_ne!(buf[(x, y)].bg, RtColor::Reset, "Reset bg hole at {x},{y}");
            }
        }
        assert_eq!(buf[(0, 0)].fg, fg, "unstyled text wears surface.fg");
        assert_eq!(buf[(0, 0)].bg, bg, "unstyled text sits on surface.bg");

        let palette = paint.pane_palette().expect("colors on");
        assert_eq!(buf[(3, 0)].fg, palette[1], "SGR 31 maps through the theme");
        assert_ne!(buf[(3, 0)].fg, RtColor::Indexed(1), "not the host's red");
        assert_eq!(buf[(5, 0)].fg, palette[9], "SGR 91 maps the bright half");
        assert_eq!(buf[(11, 0)].bg, palette[1], "SGR 41 maps the bg half");
        assert_eq!(
            buf[(7, 0)].fg,
            RtColor::Indexed(196),
            "indices past the ANSI 16 pass through untouched"
        );

        // Reverse video swaps the pair at emit time: the buffer holds
        // REVERSED over the themed surface colors, never over Reset.
        assert!(buf[(9, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(9, 0)].fg, fg, "reverse swaps the themed fg");
        assert_eq!(buf[(9, 0)].bg, bg, "reverse swaps the themed bg");
    }

    #[test]
    fn no_color_pane_content_keeps_the_hosts_colors_and_none_of_the_themes() {
        let paint = Paint::without_color_for_test();
        let buf = ground_probe_buffer(&paint);

        // Every themed cell flips to Reset (rule 11); the one non-Reset bg
        // is the program's own SGR 41, which passes through as written.
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let expected = if (x, y) == (11, 0) {
                    RtColor::Indexed(1)
                } else {
                    RtColor::Reset
                };
                assert_eq!(buf[(x, y)].bg, expected, "bg at {x},{y}");
            }
        }
        assert_eq!(
            buf[(0, 0)].fg,
            RtColor::Reset,
            "unstyled text is the host's"
        );
        assert_eq!(buf[(3, 0)].fg, RtColor::Indexed(1), "SGR 31 stays host red");
        assert_eq!(buf[(5, 0)].fg, RtColor::Indexed(9));
        assert_eq!(buf[(7, 0)].fg, RtColor::Indexed(196));
        assert!(buf[(9, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(9, 0)].fg, RtColor::Reset, "reverse swaps host colors");
    }

    #[test]
    fn a_runtimeless_pane_and_the_resize_transient_keep_the_themed_ground() {
        // No runtime yet (the blank pane before hydration): the fill is
        // the themed ground, and the whole canvas leaves no Reset holes.
        let tab = two_pane_tab();
        let runtimes = RuntimeRegistry::default();
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let paint = Paint::for_test();
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &paint, &mut ctx);
        })
        .unwrap();
        let buf = term.backend().buffer();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                assert_ne!(buf[(x, y)].bg, RtColor::Reset, "Reset bg hole at {x},{y}");
            }
        }
        let ground = theme::pane_cell(&paint);
        assert_eq!(
            buf[(5, 2)].bg,
            ground.bg.unwrap(),
            "a blank pane body sits on surface.bg"
        );

        // The resize transient: a grid smaller than the slot repaints the
        // surplus cells with the same ground, not last frame's leftovers.
        let rt = crate::runtime::PaneRuntime::new(4, 2);
        let backend = TestBackend::new(8, 4);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            paint_pane_cells(
                &rt,
                None,
                f.area(),
                f.buffer_mut(),
                theme::pane_cell(&paint),
                theme::selection_highlight(&paint),
                paint.pane_palette().as_ref(),
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        for (x, y) in [(6u16, 1u16), (2, 3), (7, 3)] {
            assert_eq!(buf[(x, y)].symbol(), " ", "surplus cell at {x},{y}");
            assert_eq!(buf[(x, y)].fg, ground.fg.unwrap());
            assert_eq!(buf[(x, y)].bg, ground.bg.unwrap());
        }
    }

    #[test]
    fn a_scrolled_pane_shows_a_depth_hint_that_leaves_with_the_scroll() {
        let tab = two_pane_tab();
        let mut runtimes = RuntimeRegistry::default();
        let mut rt = crate::runtime::PaneRuntime::new(38, 4);
        for i in 0..10 {
            rt.feed(format!("line{i}\r\n").as_bytes());
        }
        rt.scroll(-5);
        runtimes.insert("%0".to_string(), rt);
        let paint = Paint::for_test();

        let draw = |runtimes: &RuntimeRegistry| -> String {
            let backend = TestBackend::new(40, 12);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| {
                let mut hits = HitMap::default();
                let paused = std::collections::HashSet::new();
                let dec = DecorationSnapshot::default();
                let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
                paint_window(&tab, runtimes, f.area(), f.buffer_mut(), &paint, &mut ctx);
            })
            .unwrap();
            flatten(term.backend().buffer())
        };

        assert!(
            draw(&runtimes).contains("5 back"),
            "a scrolled pane must say how far back it sits"
        );

        // Back at the tail the hint is gone.
        runtimes.get_mut("%0").unwrap().scroll(1000);
        assert!(
            !draw(&runtimes).contains("back"),
            "the hint must vanish once the pane is at the live tail"
        );
    }

    #[test]
    fn gutter_ring_and_tab_bar_render() {
        let tab = two_pane_tab();
        let runtimes = RuntimeRegistry::default();

        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|f| {
            let area = f.area();
            let tab_area = Rect::new(area.x, area.y, area.width, 1);
            let canvas = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
            paint_tab_bar(
                std::slice::from_ref(&tab),
                0,
                tab_area,
                f.buffer_mut(),
                &theme,
                &mut hits,
                &DecorationSnapshot::default(),
                None,
            );
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
            paint_window(&tab, &runtimes, canvas, f.buffer_mut(), &theme, &mut ctx);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let row0: String = (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            row0.contains("main"),
            "active tab label should render: {row0}"
        );
        // Canvas starts at row 1 and panes are inset one margin cell, so
        // the focused pane's ring corners land on the canvas edge rows.
        // %0 is the active pane, so its ring is the double set.
        assert_eq!(buf[(0, 1)].symbol(), "╔", "ring top-left corner");
        assert_eq!(buf[(39, 1)].symbol(), "╗", "ring top-right corner");
        // Each stacked pane keeps its border, but the old blank row between
        // those borders is gone.
        assert_eq!(buf[(0, 6)].symbol(), "╚", "first pane bottom corner");
        assert_eq!(buf[(5, 6)].symbol(), "═", "first pane bottom border");
        assert_eq!(buf[(5, 7)].symbol(), "─", "second pane top border");
        assert_ne!(
            buf[(5, 6)].fg,
            buf[(5, 7)].fg,
            "focused and inactive borders use distinct theme tokens"
        );
        // The compact two-cell band stays grabbable for resize.
        assert!(matches!(hits.hit(20, 6), Some(HitTarget::Divider { .. })));
        assert!(matches!(hits.hit(20, 7), Some(HitTarget::Divider { .. })));
        // The bottom margin carries the second pane's muted border.
        assert_eq!(buf[(20, 11)].symbol(), "─", "every pane has a border");
    }

    /// The stacked-seam regression behind the corner grip: with a LABELED
    /// bottom pane (the normal Cyclops case, and the one the older
    /// unlabeled-pane test above never covered) the seam rows must stay
    /// resize handles wherever no visible control sits, and the swap
    /// pickup must be exactly the one-cell corner grip.
    #[test]
    fn a_labeled_bottom_pane_keeps_its_seam_for_resize_and_gets_a_corner_grip() {
        use crate::decoration::PaneDecoration;
        use cyclops_proto::AgentState;

        let tab = two_pane_tab();
        let runtimes = RuntimeRegistry::default();
        let mut decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        decoration.panes.insert(
            "%1".into(),
            PaneDecoration {
                pane_id: "%1".into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: None,
                manifest_display_name: None,
                state: AgentState::Idle,
                needs_attention: false,
            },
        );

        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let paint = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|f| {
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &paint, &mut ctx);
        })
        .unwrap();
        let buf = term.backend().buffer();

        // The seam between %0 (1,1,38,4) and %1 (1,7,38,3): row 5 is %0's
        // bottom border, row 6 is %1's top border carrying the title.
        assert!(
            matches!(hits.hit(20, 5), Some(HitTarget::Divider { .. })),
            "row A must stay a resize handle under a labeled pane"
        );
        // The minimize control owns the left end of this border now, and
        // the title strip follows it, so no single column here is reliably
        // the seam. What must hold is that the control took its own cells
        // and the seam kept enough of the rest to grab, which the count
        // assertion below measures directly.
        assert!(
            matches!(
                hits.hit(1, 6),
                Some(HitTarget::PaneMinimize { pane_id }) if pane_id == "%1"
            ),
            "the left end of the border is the minimize control"
        );
        // The title strip keeps its click target (focus, or the eye when
        // it is on); `app` never picks a `PaneFrame` up, so this cell can
        // no longer shadow the seam with a swap drag.
        assert!(matches!(
            hits.hit(20, 6),
            Some(HitTarget::PaneFrame { pane_id }) if pane_id == "%1"
        ));
        // It shadows the seam for `hit`, though, which is why `app` asks
        // `divider_at` as well: the row under the title is still the seam
        // between the two panes, and pressing it has to move that seam.
        assert_eq!(
            hits.divider_at(20, 6).map(|(pane, _)| pane),
            Some("%0"),
            "the seam under the title strip must still be grabbable"
        );

        // Counted, not sampled. The version this replaced left exactly one
        // cell of the lower pane's top border grabbable, so the pane could
        // only be resized from its far edge, and a spot check on the right
        // cell would have passed anyway.
        let grabbable = |row: u16| {
            (0..40)
                .filter(|x| hits.divider_at(*x, row).is_some())
                .count()
        };
        assert!(
            grabbable(6) > 30,
            "only {} cells of the lower pane's top border can grab the seam",
            grabbable(6)
        );
        assert!(grabbable(5) > 30, "the upper pane's bottom border too");

        // The swap handle: three cells on each frame's TOP border, beside
        // the split controls. Found through the hit map rather than at a
        // column written here, so moving the control row again does not
        // silently pass this.
        let grip_of = |want: &str| {
            (0..40u16)
                .flat_map(|x| (0..12u16).map(move |y| (x, y)))
                .find(|&(x, y)| {
                    matches!(hits.hit(x, y), Some(HitTarget::PaneGrip { pane_id }) if pane_id == want)
                })
                .unwrap_or_else(|| panic!("{want} has a grip"))
        };
        let (gx, gy) = grip_of("%0");
        assert_eq!(gy, 0, "the focused pane's grip is on its top border");
        let painted: String = (gx..gx + 3).map(|x| buf[(x, gy)].symbol()).collect();
        assert_eq!(painted, PANE_GRIP, "and it paints where it answers");
        let (_, gy1) = grip_of("%1");
        assert!(gy1 > gy, "the lower pane's grip is on its own top border");
    }

    /// The minimize control appears only on a pane that can actually
    /// shrink, and says which way the click will move it.
    ///
    /// A pane spanning the whole canvas height is the only pane in its
    /// column. `resize-pane -y` has nothing to take from it, so a control
    /// there would sit in the border collecting clicks and doing nothing,
    /// which is the exact failure this chrome language exists to avoid and
    /// the one the swap grip was already moved for.
    #[test]
    fn only_a_pane_with_room_to_give_offers_to_minimize() {
        let paint = Paint::for_test();
        let runtimes = RuntimeRegistry::default();

        let render = |tab: &crate::model::TabModel,
                      minimized: &std::collections::HashMap<String, u16>| {
            let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                let paused = std::collections::HashSet::new();
                let dec = DecorationSnapshot::default();
                let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
                ctx.minimized = minimized;
                paint_window(tab, &runtimes, f.area(), f.buffer_mut(), &paint, &mut ctx);
            })
            .unwrap();
            (term.backend().buffer().clone(), hits)
        };

        let none = std::collections::HashMap::new();
        let stacked = two_pane_tab();
        let (buf, hits) = render(&stacked, &none);

        // Stacked panes can each give rows to the other, so both offer it.
        let offered: Vec<&str> = (0..40u16)
            .flat_map(|x| (0..12u16).map(move |y| (x, y)))
            .filter_map(|(x, y)| match hits.hit(x, y) {
                Some(HitTarget::PaneMinimize { pane_id }) => Some(pane_id.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            offered.contains(&"%0") && offered.contains(&"%1"),
            "both stacked panes can shrink: {offered:?}"
        );
        let flat = flatten(&buf);
        assert!(flat.contains(PANE_MINIMIZE), "and say so: {flat}");
        assert!(
            !flat.contains(PANE_RESTORE),
            "neither is collapsed, so neither offers to expand"
        );

        // Already collapsed: the chevron turns around.
        let mut down = std::collections::HashMap::new();
        down.insert("%1".to_string(), 9u16);
        let (buf, _) = render(&stacked, &down);
        assert!(
            flatten(&buf).contains(PANE_RESTORE),
            "a collapsed pane offers to come back: {}",
            flatten(&buf)
        );

        // A single full-height pane has nowhere to put the rows.
        let solo = single_pane_tab();
        let (buf, hits) = render(&solo, &none);
        assert!(
            (0..40u16)
                .flat_map(|x| (0..12u16).map(move |y| (x, y)))
                .all(|(x, y)| !matches!(hits.hit(x, y), Some(HitTarget::PaneMinimize { .. }))),
            "a pane with no sibling to give rows to offers nothing"
        );
        assert!(
            !flatten(&buf).contains(PANE_MINIMIZE),
            "and paints nothing either: {}",
            flatten(&buf)
        );
    }

    /// Motion actually moves something now.
    ///
    /// The clock, the easing and the color interpolator all shipped built
    /// and wired to the event loop, and no painter read any of it: turning
    /// motion on scheduled 16ms wakes that composed frames identical to the
    /// ones before them. This is the first painter to read the clock, so it
    /// is also the first test that could have caught that.
    ///
    /// Mid-fade the border must be neither endpoint. The glyph set is
    /// checked to be already at its destination in the same frame, because
    /// weight is the encoding that has to survive NO_COLOR and must never
    /// be caught half way.
    #[test]
    fn a_focus_change_fades_the_border_rather_than_snapping_it() {
        use crate::animate::{Motion, MotionFrame, Seen};
        use std::time::Duration;
        // The clock runs on tokio's Instant, the same one the event loop
        // arms its deadlines with.
        use tokio::time::Instant;

        let mut paint = Paint::for_test();
        paint.truecolor = true;
        let tab = two_pane_tab();
        // `two_pane_tab` focuses %0; the grip tests above rely on the same.
        let focused = "%0".to_string();

        let mut motion = Motion::new(true);
        let start = Instant::now();
        // First frame establishes what was on screen; the second moves
        // focus, which is what arms the fade.
        motion.observe(Seen::new(None, Vec::new(), None), start);
        motion.observe(Seen::new(Some(focused.clone()), Vec::new(), None), start);

        let border_at = |motion: &Motion, at: Instant| {
            let runtimes = RuntimeRegistry::default();
            let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
            term.draw(|f| {
                let mut hits = HitMap::default();
                let paused = std::collections::HashSet::new();
                let dec = DecorationSnapshot::default();
                let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
                ctx.motion = MotionFrame::new(motion, at);
                paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &paint, &mut ctx);
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        // A cell on the focused pane's own top border, left of its controls.
        let probe = (2u16, 0u16);
        let mid = border_at(&motion, start + Duration::from_millis(60));
        let done = border_at(
            &motion,
            start + crate::animate::FOCUS + Duration::from_millis(1),
        );

        let rest = theme::pane_border(&paint).fg;
        let lit = theme::pane_border_focused(&paint).fg;
        assert_ne!(rest, lit, "the two ends must differ or this proves nothing");

        assert_eq!(
            done[probe].style().fg,
            lit,
            "the fade has to arrive at the focused color"
        );
        let half = mid[probe].style().fg;
        assert_ne!(half, rest, "and leave the resting color behind");
        assert_ne!(half, lit, "without jumping straight to the end");

        // Weight is not interpolated: it is already heavy mid-fade.
        assert_eq!(
            mid[probe].symbol(),
            done[probe].symbol(),
            "the glyph set flips at once, only the color crosses over"
        );
    }

    /// The grip must read as a handle in every shipped theme and under
    /// NO_COLOR: only its color may change, never the glyph, the same
    /// contract the status-glyph stability test below pins.
    #[test]
    fn the_grip_glyph_is_stable_across_theme_and_no_color() {
        let render_with = |paint: &Paint| -> Buffer {
            let tab = two_pane_tab();
            let runtimes = RuntimeRegistry::default();
            let backend = TestBackend::new(40, 12);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| {
                let mut hits = HitMap::default();
                let paused = std::collections::HashSet::new();
                let dec = DecorationSnapshot::default();
                let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
                paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), paint, &mut ctx);
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        // Read off the frame: the grip is a run of cells on a top border
        // now, so the assertion looks for the glyph rather than probing a
        // corner that no longer holds it.
        for paint in [
            Paint::for_test(),
            alt_test_theme_paint(),
            Paint::without_color_for_test(),
        ] {
            let buf = render_with(&paint);
            let rows: Vec<String> = (0..12u16)
                .map(|y| (0..40u16).map(|x| buf[(x, y)].symbol()).collect())
                .collect();
            let found = rows.iter().filter(|r| r.contains(PANE_GRIP)).count();
            assert_eq!(found, 2, "one grip per pane, whatever the theme");
        }
    }

    /// Focus is not a color. The frame around the pane being worked in
    /// draws a heavier glyph set than every frame beside it, so "which
    /// pane has the keyboard" survives `NO_COLOR`, a screenshot, and a
    /// reader who cannot separate two blues (rule 11). The style carries
    /// the same answer: accent while there is color, bold once there is
    /// not.
    #[test]
    fn the_focused_frame_is_heavier_than_the_panes_around_it() {
        let render_with = |paint: &Paint| -> Buffer {
            let tab = two_pane_tab();
            let runtimes = RuntimeRegistry::default();
            let backend = TestBackend::new(40, 12);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|f| {
                let mut hits = HitMap::default();
                let paused = std::collections::HashSet::new();
                let dec = DecorationSnapshot::default();
                let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
                paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), paint, &mut ctx);
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        // %0 is `two_pane_tab`'s active pane at (1,1,38,4), so its ring is
        // the cells around that: corners (0,0) and (39,0), bottom row 5.
        // %1 sits at (1,7,38,3) with its top border on row 6.
        for paint in [
            Paint::for_test(),
            alt_test_theme_paint(),
            Paint::without_color_for_test(),
        ] {
            let buf = render_with(&paint);
            assert_eq!(buf[(0, 0)].symbol(), "╔", "focused top-left corner");
            assert_eq!(buf[(39, 0)].symbol(), "╗", "focused top-right corner");
            assert_eq!(buf[(0, 5)].symbol(), "╚", "focused bottom-left corner");
            assert_eq!(buf[(20, 0)].symbol(), "═", "focused top rule");
            assert_eq!(buf[(0, 3)].symbol(), "║", "focused left rule");

            assert_eq!(
                buf[(0, 6)].symbol(),
                "╭",
                "a pane at rest keeps the calm set"
            );
            assert_eq!(buf[(20, 6)].symbol(), "─", "and its own light rule");
            assert_eq!(buf[(0, 10)].symbol(), "╰", "down to its bottom corner");
        }

        // Color off, where a hue would have been the only difference: the
        // ring keeps a weight and the frame beside it does not.
        let plain = render_with(&Paint::without_color_for_test());
        assert_eq!(
            plain[(20, 0)].fg,
            RtColor::Reset,
            "NO_COLOR must leave no color behind to lean on"
        );
        assert!(
            plain[(20, 0)].modifier.contains(Modifier::BOLD),
            "the focused ring keeps a weight once the accent is gone"
        );
        assert!(
            !plain[(20, 6)].modifier.contains(Modifier::BOLD),
            "and a pane at rest does not, or the weight says nothing"
        );
    }

    /// The notice is chrome and only chrome. It lands on the focused
    /// frame's bottom border, where the eye already is, and NOTHING else
    /// about the frame moves: not one pane cell, not one pane rectangle.
    /// A notice that reflowed the canvas would reflow every agent's TUI
    /// underneath it.
    #[test]
    fn a_notice_paints_on_the_focused_border_and_moves_no_pane_cell() {
        let tab = two_pane_tab();
        let area = Rect::new(0, 0, 40, 12);
        let render = |notice: Option<&str>| -> (Buffer, Vec<Rect>) {
            let runtimes = RuntimeRegistry::default();
            let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            term.draw(|f| {
                let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
                ctx.notice = notice;
                paint_window(
                    &tab,
                    &runtimes,
                    area,
                    f.buffer_mut(),
                    &Paint::for_test(),
                    &mut ctx,
                );
            })
            .unwrap();
            let bodies = hits
                .regions()
                .iter()
                .filter(|region| matches!(region.target, HitTarget::PaneBody { .. }))
                .map(|region| region.rect)
                .collect();
            (term.backend().buffer().clone(), bodies)
        };

        let (quiet, quiet_bodies) = render(None);
        let (noticed, noticed_bodies) = render(Some("copied 12 characters"));

        // The focused pane is %0, the top one; its frame's bottom border is
        // the row the grip test pins at y = 5, starting one cell in from
        // the canvas margin.
        let row = |buf: &Buffer, y: u16| -> String {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect()
        };
        assert!(
            row(&noticed, 5).contains("copied 12 characters"),
            "the notice must land on the focused frame's bottom border: {}",
            row(&noticed, 5)
        );
        assert!(
            !row(&quiet, 5).contains("copied"),
            "and nowhere at all when there is nothing to say"
        );
        // The grip moved to the top border, so the bottom-right cell is a
        // plain corner again. What still matters here is that the notice
        // stops before it rather than overwriting the frame.
        assert_eq!(
            noticed[(39, 5)].symbol(),
            quiet[(39, 5)].symbol(),
            "the notice must stop short of the frame's corner"
        );

        // Every other cell is untouched, and the pane rectangles are
        // identical: appearing changed no geometry, so expiring cannot
        // either.
        for y in 0..area.height {
            for x in 0..area.width {
                if y == 5 && (1..23).contains(&x) {
                    continue;
                }
                assert_eq!(
                    noticed[(x, y)],
                    quiet[(x, y)],
                    "the notice disturbed cell {x},{y}"
                );
            }
        }
        assert_eq!(
            noticed_bodies, quiet_bodies,
            "pane rectangles must not move"
        );
    }

    /// A border with no room for the whole phrase shows none of it: half a
    /// sentence on a border reads as corruption, not as feedback.
    #[test]
    fn a_notice_too_wide_for_the_border_is_not_painted_at_all() {
        let tab = two_pane_tab();
        let area = Rect::new(0, 0, 40, 12);
        let runtimes = RuntimeRegistry::default();
        let mut term = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut hits = HitMap::default();
        let paused = std::collections::HashSet::new();
        let dec = DecorationSnapshot::default();
        let long = "copied 2000 characters from a pane far too narrow to say so";
        term.draw(|f| {
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
            ctx.notice = Some(long);
            paint_window(
                &tab,
                &runtimes,
                area,
                f.buffer_mut(),
                &Paint::for_test(),
                &mut ctx,
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let border: String = (0..area.width).map(|x| buf[(x, 5)].symbol()).collect();
        assert!(
            !border.contains("copied"),
            "a clipped notice must not paint at all: {border}"
        );
    }

    /// Showing and hiding the strip: the painted frame and the
    /// tmux-declared size move together, because both come from one chrome
    /// split over one preference. If a call site ever read visibility from
    /// somewhere else, the panes would paint one row off the declared grid
    /// (the bug class of 626ec09). A lone tab keeps its strip either way,
    /// because the `+` that makes the second tab lives there.
    #[test]
    fn hiding_the_tab_bar_keeps_painted_rows_and_declared_size_together() {
        let area = Rect::new(0, 0, 40, 12);
        let tabs = vec![two_pane_tab()];

        // One frame, the way `draw` composes it: chrome split, declared
        // size, tab bar, and window paint all from the same inputs.
        let frame = |tab_bar_visible: bool| -> (Buffer, Rect, (u16, u16)) {
            // Sidebar collapsed, so its rail owns column 0 and the canvas
            // starts at column 1: the two chrome edges compose, and the
            // ring below is asserted against the split rather than against
            // a hardcoded corner.
            let areas = crate::render::chrome_areas_for(area, false, 22, tab_bar_visible);
            let declared = tmux_client_size(areas.canvas, &tabs[0]);
            let backend = TestBackend::new(area.width, area.height);
            let mut term = Terminal::new(backend).unwrap();
            let theme = Paint::for_test();
            let mut hits = HitMap::default();
            let runtimes = RuntimeRegistry::default();
            term.draw(|f| {
                paint_tab_bar(
                    &tabs,
                    0,
                    areas.tab_bar,
                    f.buffer_mut(),
                    &theme,
                    &mut hits,
                    &DecorationSnapshot::default(),
                    None,
                );
                let paused = std::collections::HashSet::new();
                let dec = DecorationSnapshot::default();
                let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
                paint_window(
                    &tabs[0],
                    &runtimes,
                    areas.canvas,
                    f.buffer_mut(),
                    &theme,
                    &mut ctx,
                );
            })
            .unwrap();
            (term.backend().buffer().clone(), areas.canvas, declared)
        };
        let top_row = |buf: &Buffer| -> String {
            (0..buf.area.width)
                .map(|x| buf[(x, 0)].symbol().to_string())
                .collect()
        };

        // Shown, which is what a fresh install gets: the strip owns the top
        // row, chip and all, and the pane ring starts under it.
        let (with_bar, canvas_with_bar, declared_with_bar) = frame(true);
        assert!(
            top_row(&with_bar).contains("main"),
            "the strip must carry the tab chips: {}",
            top_row(&with_bar)
        );
        assert!(
            top_row(&with_bar).contains('+'),
            "and the button that makes tabs: {}",
            top_row(&with_bar)
        );
        assert_eq!(
            with_bar[(canvas_with_bar.x, canvas_with_bar.y)].symbol(),
            "\u{2554}",
            "the ring has to start exactly where the chrome split put the canvas"
        );

        // Hidden on purpose: the ring reclaims the top row, no chip
        // remains, and the declared grid grows by exactly that row.
        let (no_bar, canvas_no_bar, declared_no_bar) = frame(false);
        assert_eq!(canvas_no_bar.y, canvas_with_bar.y - 1);
        assert_eq!(
            no_bar[(canvas_no_bar.x, canvas_no_bar.y)].symbol(),
            "\u{2554}",
            "the canvas reclaims the top row"
        );
        assert!(
            !top_row(&no_bar).contains("main"),
            "no tab chip may survive the hide: {}",
            top_row(&no_bar)
        );
        assert_eq!(declared_no_bar.0, declared_with_bar.0);
        assert_eq!(
            declared_no_bar.1,
            declared_with_bar.1 + 1,
            "the bar row moves between chrome and the declared grid, whole"
        );
    }

    /// A column split three ways spends more rows on separator bands than
    /// its single-pane sibling, so tmux's equal columns paint unequal boxes
    /// and the deep one reads as overhanging the rest.
    #[test]
    fn outermost_boxes_close_on_one_shared_bottom_edge() {
        let slots = vec![
            frame_slot("%0", Rect::new(0, 0, 40, 18)),
            frame_slot("%1", Rect::new(42, 0, 40, 5)),
            frame_slot("%2", Rect::new(42, 7, 40, 5)),
            frame_slot("%3", Rect::new(42, 14, 40, 6)),
        ];
        let frames = outer_frames(&slots);

        let shallow = frames[0].y + frames[0].height;
        let deep = frames[3].y + frames[3].height;
        assert_eq!(
            shallow, deep,
            "the single-pane column must close on the same row as the three-pane column"
        );
        assert_eq!(
            frames[0].width, 40,
            "a pane with a sibling beyond it keeps its own right edge"
        );
        assert_eq!(
            frames[1].height, 5,
            "a pane with a sibling below it keeps its own bottom edge"
        );
        for (slot, frame) in slots.iter().zip(&frames) {
            assert!(
                frame.width >= slot.rect.width && frame.height >= slot.rect.height,
                "a frame only ever grows past its content, never crops it"
            );
        }
    }

    /// The identical bug on the other axis: a row split three ways beside a
    /// single pane must still close on one right edge.
    #[test]
    fn outermost_boxes_close_on_one_shared_right_edge() {
        let slots = vec![
            frame_slot("%0", Rect::new(0, 0, 18, 40)),
            frame_slot("%1", Rect::new(0, 42, 5, 40)),
            frame_slot("%2", Rect::new(7, 42, 5, 40)),
            frame_slot("%3", Rect::new(14, 42, 6, 40)),
        ];
        let frames = outer_frames(&slots);

        assert_eq!(
            frames[0].x + frames[0].width,
            frames[3].x + frames[3].width,
            "both rows must end on the same column"
        );
        assert_eq!(
            frames[1].width, 5,
            "a pane with a sibling beyond it keeps its own right edge"
        );
    }

    #[test]
    fn pane_canvas_insets_by_the_margin() {
        assert_eq!(pane_canvas(Rect::new(0, 1, 40, 11)), Rect::new(1, 2, 38, 9));
        // Too small to inset: the canvas is used as-is.
        assert_eq!(pane_canvas(Rect::new(0, 0, 2, 2)), Rect::new(0, 0, 2, 2));
    }

    #[test]
    fn client_size_reserves_the_compact_internal_gutter() {
        let tab = two_pane_tab();
        assert_eq!(tmux_client_size(Rect::new(0, 1, 40, 11), &tab), (38, 8));
    }

    /// The same guarantee as the sidebar test above, for the other compact
    /// surface rule 11 names explicitly: an inactive pane's border, which
    /// (per `inactive_pane_chrome_keeps_status_compact`) paints the glyph
    /// alone with no word to fall back on.
    #[test]
    fn inactive_pane_border_glyph_is_stable_across_theme_and_no_color() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let render_with = |paint: &Paint, state: AgentState, needs_attention: bool| -> Buffer {
            let tab = two_pane_tab();
            let mut decoration = DecorationSnapshot {
                online: true,
                ..Default::default()
            };
            decoration.panes.insert(
                "%1".into(),
                PaneDecoration {
                    pane_id: "%1".into(),
                    window_id: "@0".into(),
                    label: Some("reviewer".into()),
                    manifest: None,
                    manifest_display_name: None,
                    state,
                    needs_attention,
                },
            );
            let backend = TestBackend::new(40, 12);
            let mut term = Terminal::new(backend).unwrap();
            term.draw(|frame| {
                let runtimes = RuntimeRegistry::default();
                let mut hits = HitMap::default();
                let paused = std::collections::HashSet::new();
                let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
                paint_window(
                    &tab,
                    &runtimes,
                    frame.area(),
                    frame.buffer_mut(),
                    paint,
                    &mut ctx,
                );
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        fn find_glyph(buf: &Buffer, glyph: &str) -> (u16, u16) {
            for y in 0..buf.area.height {
                for x in 0..buf.area.width {
                    if buf[(x, y)].symbol() == glyph {
                        return (x, y);
                    }
                }
            }
            panic!("glyph {glyph:?} was not painted anywhere: {}", flatten(buf));
        }

        let alt_paint = alt_test_theme_paint();
        let default_paint = Paint::for_test();
        let plain_paint = Paint::without_color_for_test();

        for (state, needs_attention, glyph) in [
            (AgentState::Idle, false, "○"),
            (AgentState::Working, false, "●"),
            (AgentState::BlockedPermission, true, "⚠"),
            (AgentState::Dead, false, "✕"),
        ] {
            let default_buf = render_with(&default_paint, state, needs_attention);
            let alt_buf = render_with(&alt_paint, state, needs_attention);
            let plain_buf = render_with(&plain_paint, state, needs_attention);

            let pos = find_glyph(&default_buf, glyph);
            assert_eq!(
                alt_buf[pos].symbol(),
                glyph,
                "an unrelated theme must not move or change the glyph for {state}"
            );
            assert_eq!(
                plain_buf[pos].symbol(),
                glyph,
                "NO_COLOR must not move or change the glyph for {state}"
            );
            assert_ne!(
                default_buf[pos].fg, alt_buf[pos].fg,
                "the theme change must actually repaint the color for {state}, \
             or the glyph check above proves nothing"
            );
            assert_eq!(
                plain_buf[pos].fg,
                RtColor::Reset,
                "NO_COLOR must leave no color behind for {state}, confirming \
             this compact cell does not depend on color to read"
            );
        }
    }

    #[test]
    fn reconnecting_pane_renders_dimmed_note() {
        let tab = two_pane_tab();
        let runtimes = RuntimeRegistry::default();
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
            ctx.link = LinkState::Reconnecting { attempt: 1 };
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(
            flat.contains("reconnecting"),
            "pane should note reconnect: {flat}"
        );
    }

    #[test]
    fn selection_highlight_renders_on_test_backend() {
        use crate::runtime::CellPos;
        use crate::selection::Selection;

        let tab = two_pane_tab();
        let mut runtimes = RuntimeRegistry::default();
        let mut rt = crate::runtime::PaneRuntime::new(5, 2);
        rt.feed(b"hello\r\n");
        runtimes.insert("%0".into(), rt);
        let sel = Selection {
            pane_id: "%0".into(),
            from: CellPos { col: 0, row: 0 },
            to: CellPos { col: 2, row: 0 },
        };
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
            ctx.selection = Some(&sel);
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
        })
        .unwrap();
        let buf = term.backend().buffer();
        // The pane sits one margin cell in from the canvas origin, so the
        // selection's cols 0..=2 of row 0 land at (1..=3, 1). "Any bg is
        // set" went vacuous once the pane ground itself carries a bg, so
        // compare against that ground instead.
        let ground = theme::pane_cell(&theme).bg.expect("themed pane ground");
        let highlight = buf[(1, 1)].bg;
        assert_ne!(
            highlight, ground,
            "selection must stand off the themed pane ground"
        );
        for x in 1..=3 {
            assert_eq!(buf[(x, 1)].bg, highlight, "selection spans its range");
        }
        assert_eq!(
            buf[(4, 1)].bg,
            ground,
            "cells past the selection keep the pane ground"
        );
    }

    #[test]
    fn agent_badge_renders_on_pane_overlay() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let tab = two_pane_tab();
        let runtimes = RuntimeRegistry::default();
        let mut decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        decoration.panes.insert(
            "%0".into(),
            PaneDecoration {
                pane_id: "%0".into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: Some("claude".into()),
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::Working,
                needs_attention: false,
            },
        );
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|f| {
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
        })
        .unwrap();
        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("working"), "badge word should render: {flat}");
        // Past the minimize control, which owns columns 1 to 3 of every
        // top border that can be collapsed.
        assert!(matches!(
            hits.hit(6, 0),
            Some(HitTarget::PaneFrame { pane_id }) if pane_id == "%0"
        ));
    }

    #[test]
    fn unknown_is_not_painted_in_primary_pane_chrome() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let node = parse_layout("0000,30x5,0,0,0").unwrap();
        let tab = TabModel {
            window_id: "@0".into(),
            name: "1".into(),
            layout: resolve_layout(&node, &[]).unwrap(),
            active_pane: "%0".into(),
            zoomed: false,
        };
        let mut decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        decoration.panes.insert(
            "%0".into(),
            PaneDecoration {
                pane_id: "%0".into(),
                window_id: "@0".into(),
                label: Some("planner".into()),
                manifest: Some("claude".into()),
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::Unknown,
                needs_attention: false,
            },
        );
        let backend = TestBackend::new(32, 7);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|frame| {
            let runtimes = RuntimeRegistry::default();
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(
                &tab,
                &runtimes,
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut ctx,
            );
        })
        .unwrap();

        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("planner"));
        assert!(!flat.contains("unknown"));
        assert!(!flat.contains("? "));
    }

    #[test]
    fn inactive_pane_chrome_keeps_status_compact() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let tab = two_pane_tab();
        let mut decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        decoration.panes.insert(
            "%1".into(),
            PaneDecoration {
                pane_id: "%1".into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: Some("claude".into()),
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::Idle,
                needs_attention: false,
            },
        );
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|frame| {
            let runtimes = RuntimeRegistry::default();
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(
                &tab,
                &runtimes,
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut ctx,
            );
        })
        .unwrap();

        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("reviewer · ○"), "compact chrome: {flat}");
        assert!(!flat.contains("reviewer · ○ idle"), "inactive word: {flat}");
    }

    #[test]
    fn narrow_pane_badge_never_overwrites_split_controls() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let node = parse_layout("0000,10x3,0,0,0").unwrap();
        let tab = TabModel {
            window_id: "@0".into(),
            name: "main".into(),
            layout: resolve_layout(&node, &[]).unwrap(),
            active_pane: "%0".into(),
            zoomed: false,
        };
        let mut decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        decoration.panes.insert(
            "%0".into(),
            PaneDecoration {
                pane_id: "%0".into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: Some("claude".into()),
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::BlockedPermission,
                needs_attention: true,
            },
        );
        let runtimes = RuntimeRegistry::default();
        let backend = TestBackend::new(12, 5);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        term.draw(|f| {
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
        })
        .unwrap();

        let row: String = (0..12)
            .map(|x| term.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(row.contains("[|][-]"), "controls stay intact: {row:?}");
        assert!(matches!(
            hits.hit(6, 0),
            Some(HitTarget::PaneSplitRight { .. })
        ));
        assert!(matches!(
            hits.hit(9, 0),
            Some(HitTarget::PaneSplitDown { .. })
        ));
    }

    #[test]
    fn compact_pane_title_keeps_the_state_glyph_visible() {
        use crate::decoration::{DecorationSnapshot, PaneDecoration};
        use cyclops_proto::AgentState;

        let node = parse_layout("0000,18x3,0,0,0").unwrap();
        let tab = TabModel {
            window_id: "@0".into(),
            name: "1".into(),
            layout: resolve_layout(&node, &[]).unwrap(),
            active_pane: "%0".into(),
            zoomed: false,
        };
        let mut decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        decoration.panes.insert(
            "%0".into(),
            PaneDecoration {
                pane_id: "%0".into(),
                window_id: "@0".into(),
                label: Some("reviewer".into()),
                manifest: Some("claude".into()),
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::BlockedPermission,
                needs_attention: true,
            },
        );
        let backend = TestBackend::new(20, 5);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        term.draw(|frame| {
            let runtimes = RuntimeRegistry::default();
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let mut ctx = ctx_defaults(&mut hits, &paused, &decoration);
            paint_window(
                &tab,
                &runtimes,
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut ctx,
            );
        })
        .unwrap();

        let row: String = (0..20)
            .map(|x| term.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            row.contains(" · ⚠"),
            "compact state remains visible: {row:?}"
        );
        assert!(row.contains("[|][-]"), "controls remain intact: {row:?}");
    }

    #[test]
    fn focused_pane_reports_cursor_position_and_requested_shape() {
        let tab = two_pane_tab();
        let mut runtimes = RuntimeRegistry::default();
        let mut rt = crate::runtime::PaneRuntime::new(40, 5);
        // DECSCUSR 6: a steady bar, the shape a modern editor's insert
        // mode asks for — exactly what must reach the host cursor.
        rt.feed(b"$ \x1b[6 q");
        runtimes.insert("%0".into(), rt);
        let backend = TestBackend::new(40, 12);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut cursor = None;
        term.draw(|f| {
            let mut hits = HitMap::default();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();
            let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
            paint_window(&tab, &runtimes, f.area(), f.buffer_mut(), &theme, &mut ctx);
            cursor = ctx.cursor;
        })
        .unwrap();
        // The pane sits one margin cell in from the canvas origin, and the
        // pane's requested shape rides along with the position.
        assert_eq!(
            cursor,
            Some(HostCursor {
                x: 3,
                y: 1,
                shape: crate::runtime::CursorShape::Bar,
                blink: false,
            }),
            "cursor should track the focused pane and carry its DECSCUSR"
        );
    }

    #[test]
    fn focused_pane_slots_flag() {
        let slots = layout_geometry(
            &two_pane_tab().layout,
            Rect::new(0, 0, 40, 11),
            "%0",
            PANE_GAPS,
        )
        .slots;
        assert_eq!(slots.len(), 2);
        assert!(slots[0].focused);
        assert!(!slots[1].focused);
    }

    // ------------------------------------------------- full-frame paint duration
    //
    // Review finding 5: `tests/baseline.rs` only ever timed the per-cell
    // walk (`baseline_pane_runtime_feed_and_grid_throughput`'s
    // `for_each_visible_cell` pass), never one whole frame through
    // `paint_window` — borders, the focused pane's accent ring, divider
    // hit-testing, and `paint_tab_bar` all run every frame too, around that
    // walk. `paint_window` and `TabModel`/`RuntimeRegistry` are
    // crate-private, so this lives here rather than in a `tests/` binary.

    /// Mirrors `tests/baseline.rs`'s generator of the same name: a
    /// synthetic byte stream mixing plain ASCII, SGR escapes, and wide
    /// (CJK) characters, standing in for real agent-TUI output.
    fn perf_synthetic_stream(min_len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(min_len + 256);
        let mut i = 0u32;
        while out.len() < min_len {
            let line = match i % 4 {
                0 => format!("plain line {i} of steady output text\r\n"),
                1 => format!("\x1b[1;32mbold green line {i}\x1b[0m\r\n"),
                2 => format!("\x1b[38;2;200;90;10m宽字符行 {i} 中文测试内容\x1b[0m\r\n"),
                _ => format!("\x1b[4munderline {i}\x1b[24m plain tail\r\n"),
            };
            out.extend_from_slice(line.as_bytes());
            i += 1;
        }
        out
    }

    /// `n` equal-size panes in one row, each `leaf_w x leaf_h`.
    /// `parse_layout` ignores the checksum field and `resolve_layout`'s
    /// empty `known` slice skips the known-pane guard (its own doc), so a
    /// layout string built by hand here is as valid an input as one tmux
    /// would have sent. The split node's own declared width is cosmetic:
    /// `layout_geometry` recomputes every position from the leaves' sizes,
    /// never from a parent's or a leaf's `x`/`y`.
    fn perf_row_layout(n: usize, leaf_w: u16, leaf_h: u16) -> crate::layout::ResolvedLayout {
        let node = if n == 1 {
            parse_layout(&format!("0000,{leaf_w}x{leaf_h},0,0,0")).expect("one-leaf layout")
        } else {
            let leaves: Vec<String> = (0..n)
                .map(|i| format!("{leaf_w}x{leaf_h},0,0,{i}"))
                .collect();
            let total_w = leaf_w * n as u16 + (n as u16 - 1) * PANE_GAPS.columns;
            parse_layout(&format!(
                "0000,{total_w}x{leaf_h},0,0{{{}}}",
                leaves.join(",")
            ))
            .expect("row layout")
        };
        resolve_layout(&node, &[]).expect("resolve row layout")
    }

    fn perf_n_pane_tab(n: usize, leaf_w: u16, leaf_h: u16) -> TabModel {
        TabModel {
            window_id: "@0".to_string(),
            name: "perf".to_string(),
            layout: perf_row_layout(n, leaf_w, leaf_h),
            active_pane: "%0".to_string(),
            zoomed: false,
        }
    }

    /// One runtime per pane, each fed enough mixed content to fill and
    /// scroll past its visible grid at least once.
    fn perf_runtimes_for(n: usize, cols: u16, rows: u16) -> RuntimeRegistry {
        let mut registry = RuntimeRegistry::default();
        let bytes = perf_synthetic_stream(8 * 1024);
        for i in 0..n {
            let mut rt = crate::runtime::PaneRuntime::new(cols, rows);
            rt.feed(&bytes);
            registry.insert(format!("%{i}"), rt);
        }
        registry
    }

    /// Paints full 1/4/8-pane frames (tab bar plus `paint_window`) into a
    /// real Ratatui `Buffer` and records the per-frame median over enough
    /// iterations to be stable, for a task that wants to prove frame
    /// composition itself did not regress, not just the cell walk inside
    /// it.
    #[test]
    fn full_frame_paint_duration_scales_with_pane_count() {
        const COLS_PER_PANE: u16 = 30;
        const PANE_ROWS: u16 = 48;
        const ITERS: usize = 200;

        for &n in &[1usize, 4, 8] {
            let inner_w =
                n as u16 * COLS_PER_PANE + (n.saturating_sub(1)) as u16 * PANE_GAPS.columns;
            let canvas_w = inner_w + 2 * PANE_MARGIN;
            let canvas_h = PANE_ROWS + 2 * PANE_MARGIN + 1; // +1: the tab bar row above the canvas
            let tab = perf_n_pane_tab(n, COLS_PER_PANE, PANE_ROWS);
            let runtimes = perf_runtimes_for(n, COLS_PER_PANE, PANE_ROWS);

            let backend = TestBackend::new(canvas_w, canvas_h);
            let mut term = Terminal::new(backend).unwrap();
            let theme = Paint::for_test();
            let paused = std::collections::HashSet::new();
            let dec = DecorationSnapshot::default();

            let mut durations = Vec::with_capacity(ITERS);
            for _ in 0..ITERS {
                let mut hits = HitMap::default();
                let t = std::time::Instant::now();
                term.draw(|f| {
                    let area = f.area();
                    let tab_area = Rect::new(area.x, area.y, area.width, 1);
                    let canvas = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
                    paint_tab_bar(
                        std::slice::from_ref(&tab),
                        0,
                        tab_area,
                        f.buffer_mut(),
                        &theme,
                        &mut hits,
                        &dec,
                        None,
                    );
                    let mut ctx = ctx_defaults(&mut hits, &paused, &dec);
                    paint_window(&tab, &runtimes, canvas, f.buffer_mut(), &theme, &mut ctx);
                })
                .unwrap();
                durations.push(t.elapsed());
            }
            durations.sort();
            let us = |d: std::time::Duration| d.as_secs_f64() * 1_000_000.0;
            let p10_us = us(durations[durations.len() / 10]);
            let median_us = us(durations[durations.len() / 2]);
            let p90_us = us(durations[(durations.len() * 9) / 10]);
            let max_us = us(durations[durations.len() - 1]);
            println!(
                "full_frame_paint {n}-pane: canvas={canvas_w}x{canvas_h} iters={ITERS} p10={p10_us:.1}us median={median_us:.1}us p90={p90_us:.1}us max={max_us:.1}us"
            );
        }
    }
}
