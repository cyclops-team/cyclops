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
use ratatui::style::{Modifier, Style};
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
    pub hits: &'a mut HitMap,
    pub decoration: &'a DecorationSnapshot,
    pub selection: Option<&'a Selection>,
    pub drag: Option<&'a DragState>,
    /// The hardware cursor when the focused pane shows one.
    pub cursor: Option<HostCursor>,
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
        paint_pane_frame(slot, *frame, canvas, buf, paint, ctx);
    }
    for (slot, frame) in slots.iter().zip(&frames).filter(|(slot, _)| slot.focused) {
        paint_pane_frame(slot, *frame, canvas, buf, paint, ctx);
    }
    // Shared pane borders are resize handles. Put divider regions above the
    // generic frame regions, then restore the visibly overlaid controls as
    // the most specific hit targets.
    push_divider_hits(&dividers, ctx.hits);
    for slot in &slots {
        push_pane_overlay_hits(slot, canvas, ctx.decoration, ctx.hits);
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
/// never another pane's content.
fn paint_pane_border(rect: Rect, bounds: Rect, buf: &mut Buffer, style: Style) {
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
        set(x, top, "─");
        set(x, bottom, "─");
    }
    for y in rect.y as i32..bottom {
        set(left, y, "│");
        set(right, y, "│");
    }
    set(left, top, "╭");
    set(right, top, "╮");
    set(left, bottom, "╰");
    set(right, bottom, "╯");
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

/// Paint one pane's border, optional named-agent chrome, and split controls.
/// Unnamed panes stay textually quiet; their muted boundary still makes the
/// layout legible.
fn paint_pane_frame(
    slot: &PaneSlot,
    frame: Rect,
    bounds: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    ctx: &mut WindowPaintCtx<'_>,
) {
    // The frame, not the content rect: a box grown out to the shared edge
    // must stay grabbable over the whole boundary it actually draws.
    let vis = frame;
    let border_style = if slot.focused {
        theme::pane_border_focused(paint)
    } else {
        theme::pane_border(paint)
    };
    paint_pane_border(vis, bounds, buf, border_style);

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

    // Controls live in the border instead of overwriting the first row of
    // the child TUI. They remain available on unfocused panes.
    let controls = pane_controls(slot, bounds);
    let control_left = controls.map_or(right, |controls| controls.split_right.x);
    if let Some(controls) = controls {
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
        let full_suffix = 3usize.saturating_add(Span::raw(full.as_str()).width());
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
            3u16.saturating_add(u16::try_from(Span::raw(state).width()).unwrap_or(u16::MAX))
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
}

fn pane_title_rect(slot: &PaneSlot, bounds: Rect, control_left: u16) -> Option<Rect> {
    let top = slot.rect.y.saturating_sub(1).max(bounds.y);
    let title_left = slot.rect.x.saturating_add(1);
    (title_left < control_left).then(|| Rect::new(title_left, top, control_left - title_left, 1))
}

#[derive(Debug, Clone, Copy)]
struct PaneControls {
    split_right: Rect,
    split_down: Rect,
}

fn pane_controls(slot: &PaneSlot, bounds: Rect) -> Option<PaneControls> {
    let vis = slot.rect;
    let left = vis.x.saturating_sub(1).max(bounds.x);
    let top = vis.y.saturating_sub(1).max(bounds.y);
    let right = (vis.x + vis.width).min(bounds.x + bounds.width - 1);
    if right.saturating_sub(left) < 8 {
        return None;
    }

    let split_down = Rect::new(right.saturating_sub(3), top, 3, 1);
    let split_right = Rect::new(split_down.x.saturating_sub(3), top, 3, 1);
    Some(PaneControls {
        split_right,
        split_down,
    })
}

fn push_pane_overlay_hits(
    slot: &PaneSlot,
    bounds: Rect,
    decoration: &DecorationSnapshot,
    hits: &mut HitMap,
) {
    let right = (slot.rect.x + slot.rect.width).min(bounds.x + bounds.width - 1);
    let controls = pane_controls(slot, bounds);
    let control_left = controls.map_or(right, |controls| controls.split_right.x);
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
            _ => super::cell_style(&cell, base),
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
        DragTarget::Tab { .. } => "⇄",
        DragTarget::Workspace { .. } | DragTarget::Agent { .. } => "⇅",
        DragTarget::Sidebar | DragTarget::EventPanel => "↔",
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
    use crate::render::test_support::{alt_test_theme_paint, flatten, two_pane_tab};

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
            cursor: None,
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
        assert_eq!(buf[(0, 1)].symbol(), "╭", "ring top-left corner");
        assert_eq!(buf[(39, 1)].symbol(), "╮", "ring top-right corner");
        // Each stacked pane keeps its border, but the old blank row between
        // those borders is gone.
        assert_eq!(buf[(0, 6)].symbol(), "╰", "first pane bottom corner");
        assert_eq!(buf[(5, 6)].symbol(), "─", "first pane bottom border");
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
        let mut saw_highlight = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].bg != RtColor::Reset {
                    saw_highlight = true;
                }
            }
        }
        assert!(saw_highlight, "selection should paint a highlight");
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
        assert!(matches!(
            hits.hit(3, 0),
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
