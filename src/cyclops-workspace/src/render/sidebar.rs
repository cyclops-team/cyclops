//! The workspace sidebar: the one side panel. A one-row tab header picks
//! what its body shows — the session tree (workspace rows, their expanded
//! agent rows, the live reorder-drop rule) or the event stream — over a
//! footer both tabs share. Reorder math itself (which slot a drag
//! previews) belongs to `crate::drag` and stream rows to `super::stream`;
//! this only paints what those hand it.
//!
//! Collapsing never leaves nothing behind. The panel's footer carries the
//! only pointer route to the app menu, so a collapse swaps the panel for a
//! one-column rail ([`paint_sidebar_rail`]) whose chevron brings it back.
//! Both states paint that chevron through [`paint_toggle`], so the two can
//! never disagree about how the control looks or answers a mouse.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use cyclops_ui::Record;

use crate::copy;
use crate::decoration::DecorationSnapshot;
use crate::drag::{DragState, DragTarget};
use crate::input::mouse::{HitMap, HitTarget};
use crate::model::WorkspaceRow;
use crate::persist::SidebarTab;
use crate::theme::{self, Paint};

/// The chevron that moves the sidebar. It points the way the click will
/// move the panel: `◂` on the open panel's own edge pushes it away, `▸` on
/// the collapsed rail brings it back. Same triangle family as the session
/// tree's own disclosure markers, one cell wide in every monospace font,
/// and chosen by state rather than theme, so the control reads under every
/// theme and under `NO_COLOR` (rule 11).
pub const SIDEBAR_COLLAPSE: &str = "◂";
pub const SIDEBAR_EXPAND: &str = "▸";

/// The one column at the panel's own outer edge that still answers as the
/// resize handle. The handle itself moved: it used to be this hidden
/// column, revealed as a dashed rule only once the pointer found it, one
/// cell short of the pane canvas's own visible border — which is exactly
/// why operators never found it. The primary handle is that border now
/// (`app::draw` pushes `HitTarget::SidebarDivider` over the canvas's
/// leftmost column right after `paint_window`, so it wins the column from
/// the pane frame beneath it), and this column is what is left behind: one
/// cell of fat-finger tolerance for a drag that starts a hair short of the
/// border. `paint_sidebar` pushes this column's hit here, before the
/// collapse chevron's own hit, so the chevron still wins the band it
/// claims (`paint_toggle`).
///
/// The resize itself is absolute — the sidebar's edge lands on the column
/// under the pointer (`render::sidebar_width_for_column`) — so a drag
/// begun on this tolerance column, one cell short of the border, snaps the
/// panel one cell narrower on its first step. A drag begun on the border
/// itself is snap-free: the pointer is already riding the column the width
/// maps to.
const SIDEBAR_GRAB_WIDTH: u16 = 1;

/// Render the workspace sidebar: the tab header, the selected tab's body,
/// the shared footer, and the collapse chevron on its outer edge.
pub fn paint_sidebar(
    workspaces: &[WorkspaceRow],
    active: usize,
    active_pane: &str,
    expanded_workspaces: &std::collections::HashSet<String>,
    agent_order: &[String],
    tab: SidebarTab,
    record: &Record,
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    decoration: &DecorationSnapshot,
    hover: Option<(u16, u16)>,
    drag: Option<&DragState>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    buf.set_style(area, theme::chrome_panel(paint));
    // No border down this edge. The pane canvas draws its own one column
    // over, so a rule here stood two parallel lines beside each other with
    // nothing between them, which reads as two windows parked side by side
    // rather than one workspace. Panel ground runs up to the canvas now.
    //
    // The column is still reserved: it is fat-finger tolerance for the
    // resize handle, which now lives one column further out on the pane
    // canvas's own visible border (see `SIDEBAR_GRAB_WIDTH` and
    // `paint_sidebar_resize_feedback`), and a reserved column buried under
    // a row's fill would be as hard to grab as the old hidden one was.
    // Reserving it also keeps every row of the body on the column it has
    // always been on, border or no border.
    let inner = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    let edge = Rect::new(
        area.x + area.width.saturating_sub(1),
        area.y,
        1,
        area.height,
    );
    let grab = Rect::new(
        area.x + area.width.saturating_sub(SIDEBAR_GRAB_WIDTH),
        area.y,
        SIDEBAR_GRAB_WIDTH.min(area.width),
        area.height,
    );
    hits.push(grab, HitTarget::SidebarDivider);

    // Two cells of breathing room keep workspace and agent names away from
    // the outer edge and the resize border.
    let pad = 2.min(inner.width / 2);
    let content = Rect::new(
        inner.x + pad,
        inner.y,
        inner.width.saturating_sub(pad.saturating_mul(2)),
        inner.height,
    );
    // The row both tabs stop above: the footer is shared, so neither body
    // may paint over it.
    let footer_y = inner.y + inner.height.saturating_sub(1);

    paint_tab_header(inner, tab, buf, paint, hits, decoration);
    // Tree rows that did not fit above the footer. The Stream tab does its
    // own clipping inside `super::stream`, so it reports nothing here.
    let clipped = match tab {
        SidebarTab::Sessions => paint_session_tree(
            workspaces,
            active,
            active_pane,
            expanded_workspaces,
            agent_order,
            area,
            inner,
            content,
            footer_y,
            buf,
            paint,
            hits,
            decoration,
            drag,
        ),
        SidebarTab::Stream => {
            // One cell in from the panel edge — the gutter the old
            // right-hand panel's border used to leave — and full width up
            // to the resize border. Stream rows are pre-formatted and wrap
            // hard at 22 columns, so every remaining column counts; the
            // session tree's second pad cell would cost one for nothing.
            let x = inner.x + 1.min(inner.width);
            let top = inner.y + 1.min(inner.height);
            super::stream::paint_event_stream(
                record,
                Rect::new(
                    x,
                    top,
                    inner.width.saturating_sub(1),
                    footer_y.saturating_sub(top),
                ),
                buf,
                paint,
            );
            0
        }
    };
    paint_footer(inner, content, footer_y, buf, paint, hits, hover, clipped);
    // Nothing paints the resize handle here anymore: the handle is the
    // pane canvas's own border now, one column out, and `app::draw` recolors
    // it (`paint_sidebar_resize_feedback`) in its own pass, after
    // `paint_window` has drawn that border for this frame. This panel only
    // still owns the tolerance column's hit target (`grab`, pushed above).
    // The collapse control sits centered on the panel's outer edge, on the
    // same row as the rail's chevron, so the control stays where the eye
    // left it when the panel goes; the column moves because the panel it
    // was attached to did. It takes a band of the resize divider and no
    // more: the handle still answers on every row outside the band, and
    // this hit is pushed last so the band wins its own rows.
    paint_toggle(
        buf,
        toggle_reach(edge),
        edge,
        SIDEBAR_COLLAPSE,
        paint,
        hits,
        hover,
    );
}

/// Recolor the pane canvas's own left border to show it doubles as the
/// sidebar's resize handle: while the pointer is near it, or for as long as
/// a resize drag runs.
///
/// This replaces the old dashed rule that used to reveal itself one column
/// short of that border. The rule shipped invisible in practice — the eye
/// only ever found the canvas's own border beside it, grabbed that, and
/// focused a pane instead of resizing anything — so the handle moved onto
/// the border operators already reach for. That means feedback is no
/// longer drawing a hidden line into blank ground; it is recoloring a line
/// that was already there, which is why this only ever touches `Style`
/// (`cell.set_style`) and never `cell.set_symbol`: the pane frame's own
/// glyphs (`canvas::BORDER_FOCUSED`/`BORDER_REST`) have to survive a resize
/// exactly as drawn, or the box the operator is mid-drag on visibly breaks.
///
/// `divider` is the canvas's leftmost column — the same rect `app::draw`
/// pushes as `HitTarget::SidebarDivider` right after `paint_window`, passed
/// here unchanged. The column immediately to its left is the sidebar's own
/// outer edge, still `SIDEBAR_GRAB_WIDTH` wide as fat-finger tolerance and
/// still answering the same hit target (`paint_sidebar`'s own `grab`); a
/// pointer resting there is aiming at this same handle, so it lights the
/// same cells rather than nothing.
///
/// Nothing at rest, same as before: the drag case is not redundant with the
/// hover case, because `app.hover` tracks plain motion and once a button is
/// down the pointer's events arrive as drags, so a handle keyed only on
/// hover would go out the moment it was grabbed.
pub fn paint_sidebar_resize_feedback(
    buf: &mut Buffer,
    divider: Rect,
    paint: &Paint,
    hover: Option<(u16, u16)>,
    drag: Option<&DragState>,
) {
    if divider.width == 0 || divider.height == 0 {
        return;
    }
    // The sidebar's own edge column, one cell inside the divider — the
    // tolerance column `SIDEBAR_GRAB_WIDTH` reserves in `paint_sidebar`.
    let grab = Rect::new(divider.x.saturating_sub(1), divider.y, 1, divider.height);
    let inside = |rect: Rect| {
        hover.is_some_and(|(col, row)| {
            col >= rect.x
                && col < rect.x + rect.width
                && row >= rect.y
                && row < rect.y + rect.height
        })
    };
    let pointed = inside(divider) || inside(grab);
    let resizing = drag.is_some_and(|drag| matches!(drag.target, DragTarget::Sidebar));
    if !pointed && !resizing {
        return;
    }
    let style = theme::pane_border_focused(paint);
    for row in divider.y..divider.y + divider.height {
        if let Some(cell) = buf.cell_mut((divider.x, row)) {
            cell.set_style(style);
        }
    }
}

/// The band of the panel's outer edge that answers as the collapse control,
/// centered on the edge.
///
/// The whole column cannot have it: this column is also the resize divider,
/// and a toggle that owned every row would leave no way to drag the panel
/// wider. A single cell in the corner was the other extreme and it was the
/// one shipped: a one-cell button on a thirty-row edge, which the operator
/// has to hunt for. A centered band is the middle, and the eye finds the
/// middle of an edge without looking.
fn toggle_reach(edge: Rect) -> Rect {
    const REACH: u16 = 5;
    let height = REACH.min(edge.height);
    Rect::new(
        edge.x,
        edge.y + edge.height.saturating_sub(height) / 2,
        1,
        height,
    )
}

/// Render the rail a collapsed sidebar leaves behind: one column of panel
/// ground with the chevron that reopens the panel at its foot. The whole
/// column is the hit target, because nothing else is painted there and a
/// one-cell glyph makes a miserable one-cell button.
pub fn paint_sidebar_rail(
    area: Rect,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    buf.set_style(area, theme::chrome_panel(paint));
    // The rail has no resize divider to share with, so the whole column is
    // the control. The glyph still centers, so it sits at the same height
    // as the open panel's and the eye keeps its place across a collapse.
    paint_toggle(buf, area, area, SIDEBAR_EXPAND, paint, hits, hover);
}

/// Paint one sidebar chevron centered in `hit`, register `hit` as the
/// toggle, and light `lit` while the pointer is inside `hit`.
///
/// The two rectangles come apart because the open panel and the collapsed
/// rail are not the same shape of control. The rail owns its whole column
/// and can answer on every row of it. The panel's edge is also the resize
/// divider, so only a band of it can answer, but the operator still reads
/// the edge as one object and expects the whole edge to respond. So the
/// band takes the clicks and the edge takes the light.
///
/// The glyph centers rather than sitting at the foot, in both states: an
/// edge control belongs at the middle of its edge, and centering is what
/// keeps the chevron at the same height when the panel becomes a rail.
fn paint_toggle(
    buf: &mut Buffer,
    hit: Rect,
    lit: Rect,
    glyph: &str,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    if hit.width == 0 || hit.height == 0 {
        return;
    }
    let hovered = hover.is_some_and(|(col, row)| {
        col >= hit.x && col < hit.x + hit.width && row >= hit.y && row < hit.y + hit.height
    });
    let style = if hovered {
        theme::add_button_hover(paint)
    } else {
        theme::add_button(paint)
    };
    // Feedback has to reach the whole object the pointer thinks it found.
    // Lighting the glyph alone answered a hover the operator made rows
    // away from it.
    if hovered {
        for row in lit.y..lit.y + lit.height {
            for col in lit.x..lit.x + lit.width {
                if let Some(cell) = buf.cell_mut((col, row)) {
                    cell.set_style(style);
                }
            }
        }
    }
    // Centered on the EDGE, not on the clickable band. Both states pass
    // the whole column as `lit`, so this is the one expression that puts
    // the chevron on the same row whether the panel is open or collapsed.
    // Centering on `hit` instead would shift it by a row when the band is
    // shorter than the edge. The band always contains the edge's midpoint,
    // so the painted glyph is always clickable.
    let y = lit.y + lit.height / 2;
    if let Some(cell) = buf.cell_mut((hit.x, y)) {
        cell.set_symbol(glyph);
        cell.set_style(style);
    }
    hits.push(hit, HitTarget::SidebarToggle);
}

/// The tab header: one row of chips naming what the body below shows, with
/// the workspace attention rollup after them.
///
/// It takes the row the session tree's plain "Workspaces" title used to
/// occupy rather than adding one, so the tree below keeps the exact rows —
/// and therefore the exact drag/reorder geometry — it had before the
/// stream moved into this panel.
fn paint_tab_header(
    inner: Rect,
    tab: SidebarTab,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    decoration: &DecorationSnapshot,
) {
    let row = Rect::new(inner.x, inner.y, inner.width, 1);
    let right = inner.x + inner.width;
    let lead = 1.min(inner.width);
    let chips = [
        (SidebarTab::Sessions, copy::SIDEBAR_TAB_SESSIONS),
        (SidebarTab::Stream, copy::SIDEBAR_TAB_STREAM),
    ];
    let full_widths = chips.map(|(_, label)| {
        u16::try_from(Span::raw(format!(" {label} ").as_str()).width()).unwrap_or(u16::MAX)
    });
    let full_total = full_widths
        .iter()
        .fold(lead, |total, w| total.saturating_add(*w));

    if inner.width >= full_total {
        // Both full chips, plus the lead space, fit: today's rendering,
        // unchanged. Painted as one `Paragraph` of spans so a wide sidebar
        // never pays the ellipsis tax it has no need for.
        let mut spans = vec![Span::styled(" ", theme::chrome_panel(paint))];
        let mut x = inner.x + lead;
        for ((chip, label), w) in chips.into_iter().zip(full_widths) {
            let style = if chip == tab {
                theme::tab_active(paint)
            } else {
                theme::tab_inactive(paint)
            };
            let text = format!(" {label} ");
            if x < right {
                hits.push(
                    Rect::new(x, row.y, w.min(right - x), 1),
                    HitTarget::SidebarTab { tab: chip },
                );
            }
            spans.push(Span::styled(text, style));
            x = x.saturating_add(w);
        }
        if decoration.workspace_needs_attention() {
            spans.push(Span::styled(
                " ◉",
                theme::attention_eye(paint)
                    .patch(paint.bg_token(cyclops_theme::tokens::CHROME_PANEL)),
            ));
        }
        Paragraph::new(Line::from(spans)).render(row, buf);
        return;
    }

    // Too narrow for both full chips: paint the lead space, then split
    // whatever is left evenly between the two chips (the first keeps the
    // odd cell) and ellipsize each label inside its own share, rather than
    // letting the row clip the second chip clean off. Each chip is now
    // painted as a filled rect rather than a span, because its background
    // has to run the whole width of its share even where the ellipsized
    // label falls short of it.
    buf.set_style(Rect::new(row.x, row.y, lead, 1), theme::chrome_panel(paint));
    let remaining = inner.width.saturating_sub(lead);
    let first_share = remaining.div_ceil(2);
    let shares = [first_share, remaining.saturating_sub(first_share)];
    let mut x = inner.x + lead;
    for ((chip, label), share) in chips.into_iter().zip(shares) {
        if share == 0 {
            continue;
        }
        let style = if chip == tab {
            theme::tab_active(paint)
        } else {
            theme::tab_inactive(paint)
        };
        let chip_rect = Rect::new(x, row.y, share, 1);
        buf.set_style(chip_rect, style);
        hits.push(chip_rect, HitTarget::SidebarTab { tab: chip });
        // Both cells of padding when the share can hold them; the leading
        // one goes first when it can't, because the label starting flush
        // with the chip's own edge is the smaller loss of the two.
        let (text_x, avail) = if share >= 3 {
            (x + 1, share - 2)
        } else {
            (x, share.saturating_sub(1).max(1))
        };
        let bounds = Rect::new(text_x, row.y, avail, 1);
        super::overlay_text_ellipsized(buf, bounds, text_x, row.y, label, style);
        x = x.saturating_add(share);
    }
    // The rollup only when real room is left over, never crushed flush
    // against the second chip's own edge.
    if decoration.workspace_needs_attention() && right.saturating_sub(x) >= 2 {
        let rollup_style =
            theme::attention_eye(paint).patch(paint.bg_token(cyclops_theme::tokens::CHROME_PANEL));
        super::overlay_text(buf, row, x, row.y, " ◉", rollup_style);
    }
}

/// The Sessions tab's body: workspace rows, their expanded agent rows, and
/// the live reorder-drop rule.
///
/// Returns the number of tree rows that did not fit above the footer. The
/// tree does not scroll, so that count is the only thing standing between
/// the operator and a list that stops without saying it stopped; the
/// footer paints it.
fn paint_session_tree(
    workspaces: &[WorkspaceRow],
    active: usize,
    active_pane: &str,
    expanded_workspaces: &std::collections::HashSet<String>,
    agent_order: &[String],
    area: Rect,
    inner: Rect,
    content: Rect,
    footer_y: u16,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    decoration: &DecorationSnapshot,
    drag: Option<&DragState>,
) -> usize {
    // A live workspace-row drag: which row is grabbed (dimmed in the loop
    // below) and, once the pointer is actually over this sidebar, which
    // slot it currently previews.
    let dragging_session = drag
        .filter(|d| d.is_active())
        .and_then(|d| match &d.target {
            DragTarget::Workspace { session_id, .. } => Some(session_id.as_str()),
            _ => None,
        });
    // The header owns row 0 and a blank row separates it from the tree, so
    // rows start on the row the old title left them on.
    let mut y = content.y + 2;
    // The daemon's note owns this row whether or not it has anything to
    // say. cyclopsd answers a beat or two after launch, and a row that
    // existed only while the note did would slide the whole tree up one
    // row at exactly that moment, under a pointer already on a target.
    // Same reasoning as the pane notice line (`canvas::paint_notice`):
    // chrome that appears and expires must not move what is around it.
    if !decoration.online {
        super::overlay_text_ellipsized(
            buf,
            content,
            content.x,
            y,
            "cyclopsd offline",
            theme::sidebar_row(paint),
        );
    }
    y += 1;

    let mut clipped = 0usize;
    for (i, ws) in workspaces.iter().enumerate() {
        let expanded = expanded_workspaces.contains(&ws.session_id);
        // Resolved before the room check so a clipped workspace can report
        // the agent rows it would have opened, not just itself.
        let agents = if expanded {
            decoration.agent_rows_for_window_ids(&ws.window_ids, agent_order)
        } else {
            Vec::new()
        };
        if y >= footer_y {
            clipped += 1 + agents.len();
            continue;
        }
        let marker = if expanded { "▾" } else { "▸" };
        // The color cue (dim) is redundant with a non-color one (the grip
        // glyph prefix) — see rule 11 and `theme::sidebar_row_dragging`.
        let dragging = dragging_session == Some(ws.session_id.as_str());
        let style = if dragging {
            theme::sidebar_row_dragging(paint)
        } else if i == active {
            theme::sidebar_workspace_active(paint)
        } else {
            theme::sidebar_workspace(paint)
        };
        let row = Rect::new(inner.x, y, inner.width, 1);
        buf.set_style(row, style);
        let grip = if dragging { "⇅ " } else { "" };
        super::overlay_text_ellipsized(
            buf,
            content,
            content.x,
            y,
            &format!("{grip}{marker} {}", ws.name),
            style,
        );
        hits.push(
            row,
            HitTarget::SidebarRow {
                session_id: ws.session_id.clone(),
                session: ws.name.clone(),
            },
        );
        hits.push(
            Rect::new(content.x, y, 1.min(content.width), 1),
            HitTarget::SidebarDisclosure {
                session_id: ws.session_id.clone(),
            },
        );
        y += 1;

        for agent in agents {
            if y >= footer_y {
                clipped += 1;
                continue;
            }
            let selected = i == active && agent.pane_id == active_pane;
            let row_style = if selected {
                theme::sidebar_row_active(paint)
            } else {
                theme::sidebar_row(paint)
            };
            let row = Rect::new(inner.x, y, inner.width, 1);
            buf.set_style(row, row_style);
            let name = DecorationSnapshot::sidebar_name(agent);
            let name_style = if agent.label.is_some() {
                row_style.patch(paint.role(name))
            } else {
                row_style
            };
            // Status leads the row, matching the compact roster shape
            // (`● Claude Code`). Unknown deliberately contributes no text.
            let mut x = content.x.saturating_add(3);
            if let Some(status) = DecorationSnapshot::primary_status(agent) {
                let status_style = if status.glyph == "⚠" {
                    theme::attention_eye(paint)
                } else {
                    paint.state(status.color_state)
                };
                super::overlay_text(buf, content, x, y, status.glyph, status_style);
                x = x.saturating_add(2);
            }
            super::overlay_text_ellipsized(buf, content, x, y, name, name_style);
            let order_key = DecorationSnapshot::agent_order_key(agent);
            hits.push(
                row,
                HitTarget::SidebarAgent {
                    workspace_id: ws.session_id.clone(),
                    pane_id: agent.pane_id.clone(),
                    order_key,
                },
            );
            y += 1;
        }
    }

    // The live drop preview: a full-width rule at the boundary the drag
    // currently previews, painted only once the pointer is actually over
    // this sidebar — a pointer that has strayed elsewhere (a pane, the tab
    // bar) shows no rule, matching that a release there leaves order
    // unchanged. Terminal rows have no sub-row resolution, so "between two
    // rows" is approximated as the row itself, repainted as a rule for as
    // long as the drag stays live.
    if let Some(drag) = drag.filter(|d| d.is_active() && dragging_session.is_some()) {
        if area.contains(ratatui::layout::Position::from(drag.current)) {
            let blocks = hits.workspace_blocks();
            let slot = crate::drag::slot_for_row(&blocks, drag.current.1);
            if let Some(rule_y) = crate::drag::boundary_row(&blocks, slot) {
                paint_insertion_rule(buf, inner, rule_y, paint);
            }
        }
    }

    clipped
}

/// The footer both tabs share: the application menu at left, and a matching
/// compact create button anchoring the hierarchy at bottom-right without
/// stealing the rest of the row.
///
/// Painted for the Stream tab too. The menu button is the only mouse route
/// to themes, keybinds, and detach, so hiding it behind a tab would strand
/// them.
///
/// `clipped` is how many rows the body above could not fit. Zero paints
/// nothing.
fn paint_footer(
    inner: Rect,
    content: Rect,
    footer_y: u16,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
    clipped: usize,
) {
    if inner.height < 2 {
        return;
    }
    let plus = " + ";
    let plus_width = u16::try_from(Span::raw(plus).width())
        .unwrap_or(u16::MAX)
        .min(inner.width);
    // The button block rides the panel's own right edge, not the content
    // inset: only the reserved divider column stands between the create
    // button and the pane border. The inset is breathing room for names
    // and the menu label; a control block that hung two pad cells short
    // of the edge read as misplaced rather than padded.
    let plus_x = inner
        .x
        .saturating_add(inner.width.saturating_sub(plus_width));
    // When the full label would run into the create button's cells the
    // menu shrinks to its own leading glyph — read off the real label so
    // the two copies can never drift apart — still the same control, one
    // cell wide, same style, same hit target shape.
    let full_menu_width =
        u16::try_from(Span::raw(copy::APP_MENU_BUTTON).width()).unwrap_or(u16::MAX);
    let menu_glyph = copy::APP_MENU_BUTTON
        .chars()
        .next()
        .map(String::from)
        .unwrap_or_default();
    let menu_narrow = plus_x.saturating_sub(content.x) < full_menu_width;
    let menu_text: &str = if menu_narrow {
        &menu_glyph
    } else {
        copy::APP_MENU_BUTTON
    };
    let menu_width = u16::try_from(Span::raw(menu_text).width())
        .unwrap_or(u16::MAX)
        .min(content.width);
    super::overlay_text(
        buf,
        content,
        content.x,
        footer_y,
        menu_text,
        theme::sidebar_label(paint),
    );
    hits.push(
        Rect::new(content.x, footer_y, menu_width, 1),
        HitTarget::AppMenu,
    );
    // The composer's button, immediately left of the create button.
    //
    // It lives here rather than only on the tab strip because the strip is
    // a preference an operator can turn off, and writing to an agent has
    // nothing to do with whether they want tabs. The sidebar is also where
    // the agents already are: the roster is above this row.
    let chat = " @ ";
    let chat_width = u16::try_from(Span::raw(chat).width())
        .unwrap_or(u16::MAX)
        .min(inner.width);
    // The footer drops this button before it drops the note beside it.
    //
    // Three things compete for one row and the narrow sidebar cannot hold
    // all of them. The note loses the least by staying: it is the only
    // thing that says what the create button makes, while this button has
    // two other doors, the Ctrl+B s chord and the tab strip's copy. So it
    // is painted only when the row can still hold the longest note it
    // might have to show afterwards.
    // Reserve the create button's note, and only that one. It is the note
    // that must survive: it is the sole explanation of a bare `+`, and a
    // test pins it. This button's own note is best-effort and drops out on
    // the narrowest sidebars that still show the button, which costs a
    // hover hint rather than a control. Plus one column, because the note's
    // own fit test below is a strict `<`.
    let kept_note = u16::try_from(Span::raw(copy::NEW_WORKSPACE_HINT).width()).unwrap_or(u16::MAX);
    let show_chat = plus_x.saturating_sub(content.x.saturating_add(menu_width))
        >= chat_width.saturating_add(kept_note).saturating_add(1);
    let chat_x = if show_chat {
        plus_x.saturating_sub(chat_width)
    } else {
        plus_x
    };
    // Both buttons keep one width whether or not they are pointed at, so
    // the target never moves out from under the mouse that found it.
    let on_row = |col: u16, row: u16, x: u16, w: u16| {
        row == footer_y && col >= x && col < x.saturating_add(w)
    };
    let hovered = hover
        .is_some_and(|(hover_col, hover_row)| on_row(hover_col, hover_row, plus_x, plus_width));
    // The create button wins a tie. They cannot overlap, but reading the
    // precedence off the code beats inferring it from arithmetic.
    let chat_hovered = show_chat
        && !hovered
        && hover
            .is_some_and(|(hover_col, hover_row)| on_row(hover_col, hover_row, chat_x, chat_width));
    // One slot, right-aligned against the create button, in the gutter the
    // footer already leaves between the menu label and that button. Two
    // things want it, so the precedence is fixed here: hover wins, because
    // that text explains the control the pointer is on right now, and the
    // count is still there when the pointer leaves. Either is skipped
    // rather than truncated when the sidebar is too narrow: half a phrase
    // next to a lit button teaches nothing.
    let note = if hovered {
        // Say what the button makes. A bare glyph does not.
        Some(copy::NEW_WORKSPACE_HINT.to_string())
    } else if chat_hovered {
        Some(copy::SIDEBAR_COMPOSE_HINT.to_string())
    } else if clipped > 0 {
        // The body stops at the footer and does not scroll, so without
        // this a workspace below the fold looks like one that does not
        // exist. Same job as the keybinds dialog's "1–12 / 20".
        Some(format!("+{clipped} more"))
    } else {
        None
    };
    if let Some(note) = note {
        let note_width = u16::try_from(Span::raw(note.as_str()).width()).unwrap_or(u16::MAX);
        let gutter = chat_x.saturating_sub(content.x.saturating_add(menu_width));
        if note_width < gutter {
            super::overlay_text(
                buf,
                inner,
                chat_x.saturating_sub(note_width),
                footer_y,
                &note,
                theme::sidebar_label(paint),
            );
        }
    }
    super::overlay_text(
        buf,
        inner,
        plus_x,
        footer_y,
        plus,
        if hovered {
            theme::add_button_hover(paint)
        } else {
            theme::add_button(paint)
        },
    );
    hits.push(
        Rect::new(plus_x, footer_y, plus_width, 1),
        HitTarget::NewWorkspaceButton,
    );
    if show_chat {
        super::overlay_text(
            buf,
            inner,
            chat_x,
            footer_y,
            chat,
            if chat_hovered {
                theme::add_button_hover(paint)
            } else {
                theme::add_button(paint)
            },
        );
        hits.push(
            Rect::new(chat_x, footer_y, chat_width, 1),
            HitTarget::ComposeButton,
        );
    }
}

/// The workspace-reorder drop indicator: a full-width accent rule at row
/// `y`, spanning `area`'s usable width. Called only while a workspace-row
/// drag is live and the pointer sits over the sidebar — see the call site
/// in [`paint_session_tree`].
fn paint_insertion_rule(buf: &mut Buffer, area: Rect, y: u16, paint: &Paint) {
    if area.width == 0 || y < area.y || y >= area.y + area.height {
        return;
    }
    let style = theme::drag_insertion_rule(paint);
    let rule: String = "─".repeat(area.width as usize);
    buf.set_stringn(area.x, y, &rule, area.width as usize, style);
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::style::Color as RtColor;
    use ratatui::Terminal;

    use super::*;
    use crate::render::test_support::{alt_test_theme_paint, flatten};

    /// Paint one sidebar at the default width with a given pointer and drag,
    /// and hand back the buffer plus the hit map.
    const GRAB_TEST_HEIGHT: u16 = 14;

    fn draw_with(hover: Option<(u16, u16)>, drag: Option<&DragState>) -> (Buffer, HitMap) {
        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
        let expanded = std::collections::HashSet::from(["$0".to_string()]);
        let mut term = Terminal::new(TestBackend::new(
            crate::render::SIDEBAR_MIN_WIDTH,
            GRAB_TEST_HEIGHT,
        ))
        .unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_sidebar(
                &workspaces,
                0,
                "%0",
                &expanded,
                &[],
                SidebarTab::Sessions,
                &Record::new(),
                f.area(),
                f.buffer_mut(),
                &Paint::for_test(),
                &mut hits,
                &DecorationSnapshot::default(),
                hover,
                drag,
            );
        })
        .unwrap();
        (term.backend().buffer().clone(), hits)
    }

    /// A body row of the panel's outer edge: below the tab header, above the
    /// footer, and outside the band the collapse chevron claims. Derived
    /// from the same function the painter uses, so the tests track the
    /// control rather than restating its arithmetic.
    fn a_row_the_chevron_leaves_alone() -> u16 {
        let band = toggle_reach(Rect::new(0, 0, 1, GRAB_TEST_HEIGHT));
        (1..GRAB_TEST_HEIGHT - 1)
            .find(|y| *y < band.y || *y >= band.y + band.height)
            .expect("a body row outside the chevron band")
    }

    /// The resize handle is the pane canvas's own left border now, not a
    /// rule drawn into the sidebar's blank edge. `paint_sidebar_resize_feedback`
    /// recolors that border's cells — never their glyphs — while the
    /// pointer is near it or a resize drag runs, and leaves it alone at
    /// rest.
    ///
    /// The old handle shipped invisible: the drag lived on the panel's
    /// outer column, that column painted blank ground, and the only line
    /// the eye could see was the canvas's own border one column further
    /// out. So the operator grabbed a border that focuses a pane and
    /// concluded the sidebar does not resize. The fix puts the handle on
    /// the border itself, rather than drawing a better-hidden line.
    #[test]
    fn the_resize_feedback_recolors_the_canvas_border_without_touching_its_glyphs() {
        let divider = Rect::new(20, 0, 1, GRAB_TEST_HEIGHT);
        let paint = Paint::for_test();
        let row = 3;

        // Stands in for the pane frame's own border paint (`canvas.rs`):
        // whatever glyph the frame drew there must survive the feedback
        // pass untouched, since only `Style` may change.
        let buffer_with = |hover: Option<(u16, u16)>, drag: Option<&DragState>| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 24, GRAB_TEST_HEIGHT));
            for y in 0..GRAB_TEST_HEIGHT {
                if let Some(cell) = buf.cell_mut((divider.x, y)) {
                    cell.set_symbol("│");
                    cell.set_style(theme::pane_border(&paint));
                }
            }
            paint_sidebar_resize_feedback(&mut buf, divider, &paint, hover, drag);
            buf
        };

        // `Cell::style()` always reports a concrete `fg`/`bg`/`underline_color`
        // (Reset where nothing was ever set), so comparing it against a bare
        // `Style` built by a theme function — which only sets the fields it
        // cares about — never matches on the untouched fields. `fg` is the
        // one field both `pane_border` and `pane_border_focused` set, and the
        // one this helper ever changes, so it is what these assertions read.
        let fg = |buf: &Buffer, x: u16, y: u16| buf[(x, y)].style().fg;
        assert_ne!(
            theme::pane_border(&paint).fg,
            theme::pane_border_focused(&paint).fg,
            "the two styles must actually differ, or the checks below prove nothing"
        );

        let rest = buffer_with(None, None);
        assert_eq!(
            fg(&rest, divider.x, row),
            theme::pane_border(&paint).fg,
            "at rest the border keeps its own color until the pointer asks for it"
        );

        let hot = buffer_with(Some((divider.x, row)), None);
        assert_eq!(
            hot[(divider.x, row)].symbol(),
            "│",
            "the border's own glyph must survive — only its style may change"
        );
        assert_eq!(
            fg(&hot, divider.x, row),
            theme::pane_border_focused(&paint).fg,
            "hovering the border recolors it to the handle's color"
        );

        // The tolerance column, one cell inside the border, must light the
        // border too: a pointer resting there is still aiming at this
        // handle (see `SIDEBAR_GRAB_WIDTH`).
        let tolerant = buffer_with(Some((divider.x - 1, row)), None);
        assert_eq!(
            fg(&tolerant, divider.x, row),
            theme::pane_border_focused(&paint).fg,
            "the sidebar's own edge column must light the canvas border too"
        );

        // Once the button is down the pointer reports drags, not motion, so
        // a handle keyed only on hover would go out the moment it was
        // grabbed.
        let drag = DragState::on_down(DragTarget::Sidebar, divider.x, row);
        let held = buffer_with(None, Some(&drag));
        assert_eq!(
            fg(&held, divider.x, row),
            theme::pane_border_focused(&paint).fg,
            "a live resize drag keeps the border lit with no hover at all"
        );
    }

    /// The grab column shrank to one cell: only the sidebar's own outer
    /// edge still answers as the resize handle's hit target in
    /// `paint_sidebar`. That whole column is fat-finger tolerance for the
    /// border-based handle now (`paint_sidebar_resize_feedback`), not a
    /// wider reach for a handle drawn here, so there is no second column
    /// left to grab.
    #[test]
    fn the_resize_handles_tolerance_column_is_one_cell_wide_below_the_header() {
        let width = crate::render::SIDEBAR_MIN_WIDTH;
        let row = a_row_the_chevron_leaves_alone();
        let (_, hits) = draw_with(None, None);
        assert!(
            matches!(hits.hit(width - 1, row), Some(HitTarget::SidebarDivider)),
            "the sidebar's own edge column must still grab the resize handle"
        );
        // And no further: the column inside it is the body's, same as
        // before the grab column shrank.
        assert!(!matches!(
            hits.hit(width - 2, row),
            Some(HitTarget::SidebarDivider)
        ));
    }

    /// The composer's button lives in the footer because the tab strip is
    /// a preference that can be off, and writing to an agent has nothing to
    /// do with wanting tabs. A wide sidebar shows it, answers the pointer,
    /// and says what it does.
    ///
    /// A narrow one drops it instead of the note. Three controls do not fit
    /// one short row, and the note is the only thing that explains the
    /// create button, while this one also has a chord and a strip copy.

    #[test]
    fn the_footer_chat_button_yields_to_the_note_before_it_yields_itself() {
        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
        let theme = Paint::for_test();
        let expanded = std::collections::HashSet::from(["$0".to_string()]);
        let draw = |width: u16, hover: Option<(u16, u16)>| {
            let mut term = Terminal::new(TestBackend::new(width, 8)).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_sidebar(
                    &workspaces,
                    0,
                    "%0",
                    &expanded,
                    &[],
                    SidebarTab::Sessions,
                    &Record::new(),
                    f.area(),
                    f.buffer_mut(),
                    &theme,
                    &mut hits,
                    &DecorationSnapshot::default(),
                    hover,
                    None,
                );
            })
            .unwrap();
            (term.backend().buffer().clone(), hits)
        };

        let find = |hits: &HitMap, w: u16| {
            (0..w)
                .flat_map(|x| (0..8u16).map(move |y| (x, y)))
                .find(|&(x, y)| matches!(hits.hit(x, y), Some(HitTarget::ComposeButton)))
        };

        // The DEFAULT sidebar width, not a width chosen because it passed.
        // The first version of this gate reserved a 16-column hover hint and
        // so hid the button on every real sidebar; a test at 40 columns said
        // it worked. This is no longer the clamp floor — the panel can be
        // dragged much narrower now — but the button and its note still fit
        // at the width a fresh install actually opens with.
        let (wide_rest, wide_hits) = draw(crate::render::SIDEBAR_DEFAULT_WIDTH, None);
        let chat = find(&wide_hits, crate::render::SIDEBAR_DEFAULT_WIDTH)
            .expect("the default sidebar paints the chat button");
        let (wide_hot, _) = draw(crate::render::SIDEBAR_DEFAULT_WIDTH, Some(chat));
        assert_ne!(
            wide_hot[chat].style(),
            wide_rest[chat].style(),
            "pointing at the chat button must change how it paints"
        );
        // Its own note is best-effort: at the default width the row has no
        // room for it, and losing a hover hint beats losing the control.
        // A roomier sidebar shows it.
        let (roomy, roomy_hits) = draw(30, None);
        let _ = roomy;
        let roomy_chat = find(&roomy_hits, 30).expect("a roomy sidebar paints it too");
        let (roomy_hot, _) = draw(30, Some(roomy_chat));
        assert!(
            flatten(&roomy_hot).contains(copy::SIDEBAR_COMPOSE_HINT),
            "a sidebar with room should say what it does: {}",
            flatten(&roomy_hot)
        );

        // Too narrow for all three: the button goes, the note stays. The
        // block riding the panel's right edge bought two more columns, so
        // "too narrow" sits two columns lower than it did when the block
        // stopped at the content inset.
        let (_, narrow_hits) = draw(17, None);
        assert!(
            find(&narrow_hits, 17).is_none(),
            "a narrow footer should drop the chat button"
        );
        let narrow_plus = (0..17u16)
            .flat_map(|x| (0..8u16).map(move |y| (x, y)))
            .find(|&(x, y)| matches!(narrow_hits.hit(x, y), Some(HitTarget::NewWorkspaceButton)))
            .expect("the create button survives");
        let (narrow_hot, _) = draw(17, Some(narrow_plus));
        assert!(
            flatten(&narrow_hot).contains(copy::NEW_WORKSPACE_HINT),
            "the note is what a narrow footer keeps: {}",
            flatten(&narrow_hot)
        );
    }

    /// The create button is a bare glyph at rest, so the mouse has to be
    /// what explains it: pointing at it fills the button and names what it
    /// makes, and the target must not move while being pointed at.
    #[test]
    fn sidebar_create_button_answers_the_mouse() {
        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
        let theme = Paint::for_test();
        let expanded = std::collections::HashSet::from(["$0".to_string()]);

        let draw = |hover: Option<(u16, u16)>| {
            let mut term = Terminal::new(TestBackend::new(20, 8)).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_sidebar(
                    &workspaces,
                    0,
                    "%0",
                    &expanded,
                    &[],
                    SidebarTab::Sessions,
                    &Record::new(),
                    f.area(),
                    f.buffer_mut(),
                    &theme,
                    &mut hits,
                    &DecorationSnapshot::default(),
                    hover,
                    None,
                );
            })
            .unwrap();
            let buf = term.backend().buffer().clone();
            (buf, hits)
        };

        let (rest_buf, rest_hits) = draw(None);
        let plus = (0..rest_buf.area.width)
            .flat_map(|x| (0..rest_buf.area.height).map(move |y| (x, y)))
            .find(|&(x, y)| matches!(rest_hits.hit(x, y), Some(HitTarget::NewWorkspaceButton)))
            .expect("the sidebar paints a create button");

        let (hot_buf, hot_hits) = draw(Some(plus));
        assert_eq!(
            hot_hits.hit(plus.0, plus.1).cloned(),
            rest_hits.hit(plus.0, plus.1).cloned(),
            "the button must not move out from under the mouse that found it"
        );
        assert_ne!(
            hot_buf[plus].style(),
            rest_buf[plus].style(),
            "pointing at the create button must change how it paints"
        );
        assert!(
            flatten(&hot_buf).contains(copy::NEW_WORKSPACE_HINT),
            "hovering should name what the button makes: {}",
            flatten(&hot_buf)
        );
        assert!(
            !flatten(&rest_buf).contains(copy::NEW_WORKSPACE_HINT),
            "the hint belongs to hover, not to the resting sidebar"
        );
    }

    /// Below the drag floor — which a small terminal's half-width cap in
    /// `clamp_sidebar_width` can still force — the edge-riding create
    /// button starts too few cells from the menu label's own column for
    /// the full "☰menu" (5 cells; only 4 remain at width 10), so the menu
    /// button shrinks to its own leading glyph rather than hard-clipping
    /// into the create button's territory. Both must still answer the
    /// mouse, and the two must never claim the same column.
    #[test]
    fn a_below_floor_sidebar_shrinks_the_menu_button_without_overlap() {
        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
        let expanded = std::collections::HashSet::new();
        let paint = Paint::for_test();
        // Ten columns: under the floor, reachable only through the
        // half-width cap, and the width the cell math above is counted at.
        let width = 10;
        let mut term = Terminal::new(TestBackend::new(width, 8)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_sidebar(
                &workspaces,
                0,
                "%0",
                &expanded,
                &[],
                SidebarTab::Sessions,
                &Record::new(),
                f.area(),
                f.buffer_mut(),
                &paint,
                &mut hits,
                &DecorationSnapshot::default(),
                None,
                None,
            );
        })
        .unwrap();

        // The footer row: height 8 puts it on the last row, same math
        // `paint_footer` itself uses (`footer_y = inner.y + inner.height - 1`).
        let footer_y = 7;
        let cols_hit = |matches_target: fn(&HitTarget) -> bool| -> Vec<u16> {
            (0..width)
                .filter(|&x| hits.hit(x, footer_y).is_some_and(matches_target))
                .collect()
        };
        let menu_cols = cols_hit(|h| matches!(h, HitTarget::AppMenu));
        let plus_cols = cols_hit(|h| matches!(h, HitTarget::NewWorkspaceButton));

        assert!(
            !menu_cols.is_empty(),
            "the menu button must still answer the mouse below the floor"
        );
        assert!(
            !plus_cols.is_empty(),
            "the create button must still answer the mouse below the floor"
        );
        assert!(
            menu_cols.iter().all(|c| !plus_cols.contains(c)),
            "the two buttons must never share a column: menu {menu_cols:?}, plus {plus_cols:?}"
        );

        let buf = term.backend().buffer().clone();
        assert_eq!(
            buf[(menu_cols[0], footer_y)].symbol(),
            "☰",
            "the shrunk menu button is its own leading glyph, not a clipped label"
        );
    }

    #[test]
    fn sidebar_rows_render_and_hit_test_aligned() {
        let workspaces = vec![
            WorkspaceRow {
                session_id: "$0".into(),
                name: "cyclops".into(),
                tab_count: 2,
                window_ids: vec!["@0".into()],
            },
            WorkspaceRow {
                session_id: "$1".into(),
                name: "website".into(),
                tab_count: 1,
                window_ids: vec!["@1".into()],
            },
        ];
        let backend = TestBackend::new(20, 8);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let expanded = std::collections::HashSet::from(["$0".to_string()]);
        term.draw(|f| {
            paint_sidebar(
                &workspaces,
                0,
                "%0",
                &expanded,
                &[],
                SidebarTab::Sessions,
                &Record::new(),
                f.area(),
                f.buffer_mut(),
                &theme,
                &mut hits,
                &DecorationSnapshot::default(),
                None,
                None,
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let flat = flatten(buf);
        assert!(
            flat.contains("cyclops"),
            "sidebar should list workspace: {flat}"
        );
        assert!(flat.contains('▾'), "active row should be expanded");
        assert_eq!(buf[(0, 3)].symbol(), " ", "left padding cell one");
        assert_eq!(buf[(1, 3)].symbol(), " ", "left padding cell two");
        // The tab header, a spacer, then the row reserved for the daemon
        // note, so workspaces paint on rows 3 and 4.
        assert!(matches!(
            hits.hit(3, 3),
            Some(HitTarget::SidebarRow { session, .. }) if session == "cyclops"
        ));
        assert!(matches!(
            hits.hit(3, 4),
            Some(HitTarget::SidebarRow { session, .. }) if session == "website"
        ));
        assert!(matches!(
            hits.hit(2, 3),
            Some(HitTarget::SidebarDisclosure { session_id }) if session_id == "$0"
        ));
        // Bottom row carries distinct menu and create buttons; the create
        // button rides the panel's right edge, one reserved divider column
        // short of the pane border.
        assert!(matches!(hits.hit(2, 7), Some(HitTarget::AppMenu)));
        assert!(matches!(
            hits.hit(17, 7),
            Some(HitTarget::NewWorkspaceButton)
        ));
        assert!(flat.contains("menu"), "menu button should render: {flat}");
        assert!(flat.contains('+'), "create button should render: {flat}");
    }

    /// One workspace, drawn twice: once with the daemon note showing and
    /// once without. The note's row is reserved either way.
    ///
    /// cyclopsd answers a beat or two after launch, so this flip happens on
    /// a routine boot. If the row only existed while the note did, the
    /// whole tree would jump up one row at that moment, moving every click
    /// target out from under a pointer that had already found one.
    #[test]
    fn the_daemon_note_reserves_its_row_so_the_tree_never_moves() {
        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
        let expanded = std::collections::HashSet::from(["$0".to_string()]);
        let paint = Paint::for_test();

        let render = |online: bool| {
            let decoration = DecorationSnapshot {
                online,
                ..Default::default()
            };
            let mut term = Terminal::new(TestBackend::new(24, 8)).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_sidebar(
                    &workspaces,
                    0,
                    "%0",
                    &expanded,
                    &[],
                    SidebarTab::Sessions,
                    &Record::new(),
                    f.area(),
                    f.buffer_mut(),
                    &paint,
                    &mut hits,
                    &decoration,
                    None,
                    None,
                );
            })
            .unwrap();
            (term.backend().buffer().clone(), hits)
        };

        let workspace_row = |hits: &HitMap| {
            (0..8u16)
                .find(|y| matches!(hits.hit(3, *y), Some(HitTarget::SidebarRow { .. })))
                .expect("the workspace row must paint")
        };

        let (offline_buf, offline_hits) = render(false);
        let (online_buf, online_hits) = render(true);

        // The flip is real, or the row comparison below proves nothing.
        assert!(
            flatten(&offline_buf).contains("cyclopsd offline"),
            "an offline daemon must still say so: {}",
            flatten(&offline_buf)
        );
        assert!(
            !flatten(&online_buf).contains("cyclopsd offline"),
            "the note goes when the daemon answers: {}",
            flatten(&online_buf)
        );
        assert_eq!(
            workspace_row(&offline_hits),
            workspace_row(&online_hits),
            "the daemon coming online must not move the tree"
        );
    }

    /// The tree stops at the footer and does not scroll, so what it
    /// dropped has to be said somewhere: a workspace below the fold is
    /// otherwise indistinguishable from one that does not exist.
    #[test]
    fn the_footer_counts_the_tree_rows_that_did_not_fit() {
        let workspaces: Vec<WorkspaceRow> = (0..7)
            .map(|i| WorkspaceRow {
                session_id: format!("${i}"),
                name: format!("ws{i}"),
                tab_count: 1,
                window_ids: vec![format!("@{i}")],
            })
            .collect();
        let paint = Paint::for_test();

        let render = |height: u16| {
            let mut term = Terminal::new(TestBackend::new(30, height)).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_sidebar(
                    &workspaces,
                    0,
                    "%0",
                    &std::collections::HashSet::new(),
                    &[],
                    SidebarTab::Sessions,
                    &Record::new(),
                    f.area(),
                    f.buffer_mut(),
                    &paint,
                    &mut hits,
                    &DecorationSnapshot::default(),
                    None,
                    None,
                );
            })
            .unwrap();
            flatten(term.backend().buffer())
        };

        // Eight rows: header, spacer, the reserved daemon-note row, four
        // tree rows, footer. Three of the seven workspaces miss the cut.
        let short = render(8);
        assert!(
            short.contains("ws3"),
            "the four rows that fit still paint: {short}"
        );
        assert!(
            !short.contains("ws4"),
            "row five is below the fold: {short}"
        );
        assert!(
            short.contains("+3 more"),
            "the footer must count what it dropped: {short}"
        );

        // Room for every row: the footer has nothing to report.
        let tall = render(16);
        assert!(tall.contains("ws6"), "every workspace fits: {tall}");
        assert!(
            !tall.contains("more"),
            "a count with nothing clipped is a lie: {tall}"
        );
    }

    /// The count is rows the operator cannot see, not workspaces. A
    /// clipped workspace that is expanded takes its agent rows down with
    /// it, and they are just as invisible.
    #[test]
    fn the_clipped_count_includes_the_agents_under_a_clipped_workspace() {
        use crate::decoration::PaneDecoration;
        use cyclops_proto::AgentState;

        let workspaces: Vec<WorkspaceRow> = (0..5)
            .map(|i| WorkspaceRow {
                session_id: format!("${i}"),
                name: format!("ws{i}"),
                tab_count: 1,
                window_ids: vec![format!("@{i}")],
            })
            .collect();
        let paint = Paint::for_test();

        // Two agents live in the last workspace's window — the one that
        // will not fit.
        let mut decoration = DecorationSnapshot {
            online: true,
            ..Default::default()
        };
        for pane in ["%0", "%1"] {
            decoration.panes.insert(
                pane.into(),
                PaneDecoration {
                    pane_id: pane.into(),
                    window_id: "@4".into(),
                    label: None,
                    manifest: Some("claude".into()),
                    manifest_display_name: Some("Claude Code".into()),
                    state: AgentState::Idle,
                    needs_attention: false,
                },
            );
        }

        let render = |expanded: &std::collections::HashSet<String>| {
            let mut term = Terminal::new(TestBackend::new(30, 8)).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_sidebar(
                    &workspaces,
                    0,
                    "%9",
                    expanded,
                    &[],
                    SidebarTab::Sessions,
                    &Record::new(),
                    f.area(),
                    f.buffer_mut(),
                    &paint,
                    &mut hits,
                    &decoration,
                    None,
                    None,
                );
            })
            .unwrap();
            flatten(term.backend().buffer())
        };

        // Four tree rows fit, so $4 is clipped. Collapsed it is one row.
        let collapsed = render(&std::collections::HashSet::new());
        assert!(
            collapsed.contains("+1 more"),
            "one clipped workspace, one row: {collapsed}"
        );

        // Expanded it is three: itself and the two agents under it.
        let expanded = render(&std::collections::HashSet::from(["$4".to_string()]));
        assert!(
            expanded.contains("+3 more"),
            "a clipped workspace hides its open agent rows too: {expanded}"
        );
    }

    /// A live workspace-row drag must (1) mark the grabbed row with a
    /// non-color cue on top of the color one, and (2) paint a full-width
    /// rule at the exact boundary the drag currently previews — the same
    /// slot math `commit_drag_drop` uses to resolve the drop, so what the
    /// user watches while dragging is what actually lands.
    #[test]
    fn dragging_a_workspace_row_dims_it_and_paints_the_previewed_rule() {
        let workspaces = vec![
            WorkspaceRow {
                session_id: "$0".into(),
                name: "cyclops".into(),
                tab_count: 2,
                window_ids: vec!["@0".into()],
            },
            WorkspaceRow {
                session_id: "$1".into(),
                name: "website".into(),
                tab_count: 1,
                window_ids: vec!["@1".into()],
            },
        ];
        // Both collapsed: rows 3 ($0) and 4 ($1), matching
        // `sidebar_rows_render_and_hit_test_aligned`.
        let expanded = std::collections::HashSet::new();
        let theme = Paint::for_test();

        let render = |drag: Option<&DragState>| {
            let backend = TestBackend::new(20, 8);
            let mut term = Terminal::new(backend).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_sidebar(
                    &workspaces,
                    0,
                    "%0",
                    &expanded,
                    &[],
                    SidebarTab::Sessions,
                    &Record::new(),
                    f.area(),
                    f.buffer_mut(),
                    &theme,
                    &mut hits,
                    &DecorationSnapshot::default(),
                    None,
                    drag,
                );
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        let at_rest = render(None);

        // $1 (website, row 4) is picked up and dragged onto $0's row (3) —
        // the rule should preview inserting before $0.
        let mut drag = DragState::on_down(
            DragTarget::Workspace {
                session_id: "$1".into(),
                session: "website".into(),
            },
            3,
            4,
        );
        drag.on_move(3, 3);
        assert!(drag.is_active(), "past the 1-cell sidebar row threshold");
        let dragging = render(Some(&drag));

        // (1) The grabbed row keeps its name but gains a non-color grip
        // glyph and a materially different style than at rest — color
        // alone never carries this.
        let row4 = |buf: &Buffer| {
            (0..buf.area.width)
                .map(|x| buf[(x, 4)].symbol().to_string())
                .collect::<String>()
        };
        assert!(
            row4(&dragging).contains('⇅'),
            "the grabbed row shows a non-color marker glyph: {}",
            row4(&dragging)
        );
        assert!(
            row4(&dragging).contains("website"),
            "the grabbed row's own name stays visible while dragging"
        );
        assert_ne!(
            dragging[(2, 4)].style(),
            at_rest[(2, 4)].style(),
            "the grabbed row's style must change while dragging"
        );

        // (2) The rule paints across the sidebar's usable width at row 3 —
        // the previewed boundary — and nowhere else.
        let inner_width = 19; // area width 20 minus the 1-cell resize divider
        for x in 0..inner_width {
            assert_eq!(
                dragging[(x, 3)].symbol(),
                "─",
                "the rule should span the full sidebar width at column {x}"
            );
        }
        assert_ne!(
            dragging[(inner_width, 3)].symbol(),
            "─",
            "the rule must leave the resize divider column graspable"
        );
        // Rows other than the previewed boundary are unaffected by the
        // rule (row 4 still reads as the grabbed row's own text, not a
        // second copy of the line).
        assert_ne!(dragging[(0, 4)].symbol(), "─");
    }

    #[test]
    fn sidebar_agents_use_hierarchy_display_names_and_compact_status() {
        use crate::decoration::PaneDecoration;
        use cyclops_proto::AgentState;

        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
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
                state: AgentState::Unknown,
                needs_attention: false,
            },
        );
        decoration.panes.insert(
            "%1".into(),
            PaneDecoration {
                pane_id: "%1".into(),
                window_id: "@0".into(),
                label: None,
                manifest: Some("claude".into()),
                manifest_display_name: Some("Claude Code".into()),
                state: AgentState::Working,
                needs_attention: false,
            },
        );
        let backend = TestBackend::new(28, 8);
        let mut term = Terminal::new(backend).unwrap();
        let theme = Paint::for_test();
        let mut hits = HitMap::default();
        let expanded = std::collections::HashSet::from(["$0".to_string()]);
        term.draw(|frame| {
            paint_sidebar(
                &workspaces,
                0,
                "%0",
                &expanded,
                &["pane:%1".into(), "name:reviewer".into()],
                SidebarTab::Sessions,
                &Record::new(),
                frame.area(),
                frame.buffer_mut(),
                &theme,
                &mut hits,
                &decoration,
                None,
                None,
            );
        })
        .unwrap();

        let flat = flatten(term.backend().buffer());
        assert!(flat.contains("● Claude Code"), "detected agent: {flat}");
        assert!(flat.contains("reviewer"), "named pane: {flat}");
        assert!(
            !flat.contains("unknown"),
            "unknown stays diagnostic: {flat}"
        );
        assert!(!flat.contains("? reviewer"), "unknown has no glyph: {flat}");
        // Header, spacer, the reserved daemon-note row, the workspace row:
        // the first agent lands on row 4 online or off.
        assert!(
            matches!(
                hits.hit(6, 4),
                Some(HitTarget::SidebarAgent { pane_id, .. }) if pane_id == "%1"
            ),
            "persisted agent order should put Claude first"
        );
    }

    /// Rule 11's compact glyph vocabulary is a fixed mapping from
    /// `AgentState`/attention to one of four characters (`○`, `●`, `⚠`,
    /// `✕`) — it is not a color swatch. Feed the sidebar's status cell two
    /// materially different themes plus `NO_COLOR` and the glyph at the
    /// same cell must read identically every time; only the `Style`
    /// painted under it may change (and must, on the two colored runs, or
    /// this proves nothing).
    #[test]
    fn sidebar_state_glyph_is_stable_across_theme_and_no_color() {
        use crate::decoration::PaneDecoration;
        use cyclops_proto::AgentState;

        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
        let expanded = std::collections::HashSet::from(["$0".to_string()]);

        let render_with = |paint: &Paint, state: AgentState, needs_attention: bool| -> Buffer {
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
                    manifest: None,
                    manifest_display_name: None,
                    state,
                    needs_attention,
                },
            );
            let backend = TestBackend::new(24, 8);
            let mut term = Terminal::new(backend).unwrap();
            let mut hits = HitMap::default();
            term.draw(|f| {
                paint_sidebar(
                    &workspaces,
                    0,
                    "%9",
                    &expanded,
                    &[],
                    SidebarTab::Sessions,
                    &Record::new(),
                    f.area(),
                    f.buffer_mut(),
                    paint,
                    &mut hits,
                    &decoration,
                    None,
                    None,
                );
            })
            .unwrap();
            term.backend().buffer().clone()
        };

        let alt_paint = alt_test_theme_paint();
        let default_paint = Paint::for_test();
        let plain_paint = Paint::without_color_for_test();

        // Column 5, row 4: one expanded workspace puts its first agent row
        // at y = 4 (header, blank, reserved daemon-note row, workspace
        // row), and the status glyph lands 3 cells past the 2-cell sidebar
        // pad (`content.x.saturating_add(3)` in `paint_sidebar`).
        let (gx, gy) = (5, 4);
        for (state, needs_attention, glyph) in [
            (AgentState::Idle, false, "○"),
            (AgentState::Working, false, "●"),
            (AgentState::BlockedPermission, true, "⚠"),
            (AgentState::Dead, false, "✕"),
        ] {
            let default_buf = render_with(&default_paint, state, needs_attention);
            let alt_buf = render_with(&alt_paint, state, needs_attention);
            let plain_buf = render_with(&plain_paint, state, needs_attention);

            assert_eq!(
                default_buf[(gx, gy)].symbol(),
                glyph,
                "default theme glyph for {state}"
            );
            assert_eq!(
                alt_buf[(gx, gy)].symbol(),
                glyph,
                "an unrelated theme must not change the glyph for {state}"
            );
            assert_eq!(
                plain_buf[(gx, gy)].symbol(),
                glyph,
                "NO_COLOR must not change the glyph for {state}"
            );
            assert_ne!(
                default_buf[(gx, gy)].fg,
                alt_buf[(gx, gy)].fg,
                "the theme change must actually repaint the color for {state}, \
             or the glyph check above proves nothing"
            );
            assert_eq!(
                plain_buf[(gx, gy)].fg,
                RtColor::Reset,
                "NO_COLOR must leave no color behind for {state}, confirming \
             this compact cell does not depend on color to read"
            );
        }
    }

    /// A workspace name too long for a narrow panel ends in `…` instead of
    /// stopping mid-word, and the row never writes past `content`'s own
    /// right edge — the two pad columns between `content` and the resize
    /// divider stay blank, the same as any other row.
    #[test]
    fn a_long_workspace_name_ellipsizes_instead_of_clipping_mid_word() {
        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "a-very-long-workspace-name-nobody-chose-on-purpose".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
        let expanded = std::collections::HashSet::new();
        let paint = Paint::for_test();
        let width = crate::render::SIDEBAR_MIN_WIDTH;
        let mut term = Terminal::new(TestBackend::new(width, 8)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_sidebar(
                &workspaces,
                0,
                "%0",
                &expanded,
                &[],
                SidebarTab::Sessions,
                &Record::new(),
                f.area(),
                f.buffer_mut(),
                &paint,
                &mut hits,
                &DecorationSnapshot::default(),
                None,
                None,
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();

        // Header, spacer, the reserved daemon-note row: the workspace lands
        // on row 3, same as `sidebar_rows_render_and_hit_test_aligned`.
        let row_y = 3;
        let row: String = (0..width).map(|x| buf[(x, row_y)].symbol().to_string()).collect();
        assert!(
            row.contains('…'),
            "a name too long for the panel must ellipsize: {row:?}"
        );
        assert!(
            !row.contains("purpose"),
            "the truncated tail of the name must not survive: {row:?}"
        );

        // `content` is `inner` (width − 1) inset `pad` cells each side —
        // the same arithmetic `paint_sidebar` itself uses — so its own
        // right edge sits short of the resize divider. Every column from
        // there to the divider must stay blank: the ellipsizing helper is
        // bounded by `content`, not by the wider row it paints on.
        let inner_width = width - 1;
        let pad = 2.min(inner_width / 2);
        let content_right = pad + inner_width.saturating_sub(pad.saturating_mul(2));
        for x in content_right..inner_width {
            assert_eq!(
                buf[(x, row_y)].symbol(),
                " ",
                "column {x} is past content's right edge and must stay blank"
            );
        }
    }

    // -- The tab header: two chips over one body, the resize handle and
    // the footer untouched by either. --

    /// A sidebar rectangle narrower than the terminal, so "clipped to the
    /// sidebar rect" is something a test can actually read: every column
    /// past `SIDEBAR` belongs to the pane canvas.
    const SIDEBAR: Rect = Rect {
        x: 0,
        y: 0,
        width: 42,
        height: 8,
    };

    /// One admitted stream row, short enough to render on one line at the
    /// sidebar's widest.
    fn one_row_record() -> Record {
        let mut record = Record::new();
        record.live(cyclops_ui::Entry {
            uid: 0,
            ts: 1_000,
            seq: None,
            id: None,
            kind: cyclops_ui::EntryKind::State {
                target: "rev".into(),
                pane_id: Some("%1".into()),
                state: cyclops_proto::AgentState::BlockedPermission,
            },
        });
        record
    }

    /// Paint one sidebar into a 60-column terminal and hand back the whole
    /// buffer plus the hit map, so a caller can assert on both what the
    /// sidebar painted and what it left alone.
    fn draw_sidebar(tab: SidebarTab, record: &Record, paint: &Paint) -> (Buffer, HitMap) {
        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
        let expanded = std::collections::HashSet::from(["$0".to_string()]);
        let mut term = Terminal::new(TestBackend::new(60, SIDEBAR.height)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_sidebar(
                &workspaces,
                0,
                "%0",
                &expanded,
                &[],
                tab,
                record,
                SIDEBAR,
                f.buffer_mut(),
                paint,
                &mut hits,
                &DecorationSnapshot::default(),
                None,
                None,
            );
        })
        .unwrap();
        (term.backend().buffer().clone(), hits)
    }

    /// The text inside the sidebar's own rectangle, row by row.
    fn sidebar_text(buf: &Buffer) -> String {
        (SIDEBAR.y..SIDEBAR.y + SIDEBAR.height)
            .flat_map(|y| (SIDEBAR.x..SIDEBAR.x + SIDEBAR.width).map(move |x| (x, y)))
            .map(|cell| buf[cell].symbol().to_string())
            .collect()
    }

    /// The Stream tab shows the shared stream model's rows in the sidebar
    /// and nowhere else: the session tree gives way, the row reads, the
    /// footer stays, and not one cell lands past the sidebar's rectangle —
    /// the columns the old right-hand panel used to take belong to the
    /// pane canvas now.
    #[test]
    fn the_stream_tab_paints_event_rows_inside_the_sidebar_rect() {
        let paint = Paint::for_test();
        let record = one_row_record();
        let word = cyclops_proto::AgentState::BlockedPermission.to_string();

        let (stream_buf, _) = draw_sidebar(SidebarTab::Stream, &record, &paint);
        let stream = sidebar_text(&stream_buf);
        assert!(
            stream.contains(&word),
            "the stream row must read: {stream:?}"
        );
        assert!(stream.contains("rev"), "{stream:?}");
        assert!(
            !stream.contains("cyclops"),
            "the session tree belongs to the other tab: {stream:?}"
        );
        // The button's word, not the whole label: `☰` is a wide glyph and
        // reads back with its spacer cell spliced in, which is a property
        // of this flattening and not of the footer.
        assert!(
            stream.contains("menu"),
            "the footer is shared: the app menu must survive the tab switch: {stream:?}"
        );
        for y in 0..stream_buf.area.height {
            for x in SIDEBAR.x + SIDEBAR.width..stream_buf.area.width {
                assert_eq!(
                    stream_buf[(x, y)].symbol(),
                    " ",
                    "the stream painted past the sidebar at {x},{y}"
                );
            }
        }

        // The control: the same call on the other tab shows the tree and
        // no stream row, so neither assertion above passed by accident.
        let (tree_buf, _) = draw_sidebar(SidebarTab::Sessions, &record, &paint);
        let tree = sidebar_text(&tree_buf);
        assert!(tree.contains("cyclops"), "{tree:?}");
        assert!(!tree.contains(&word), "{tree:?}");
    }

    /// Both chips answer the mouse where they paint, and the selected one
    /// is materially different from the other. Rule 11: the cue survives
    /// `NO_COLOR`, because the accent chip reverses when there is no color
    /// to fill it with — the same rule the tab strip follows.
    #[test]
    /// The header's own claim, measured at the width that actually ships
    /// (`WorkspacePrefs::default().sidebar_width` is 22, the minimum):
    /// both chips answer the mouse AND the attention rollup still paints.
    /// The wider tests above would pass with a header that silently drops
    /// the rollup on every default install.
    fn the_header_fits_both_chips_and_the_rollup_at_the_default_width() {
        let record = Record::new();
        let paint = Paint::for_test();
        let narrow = Rect::new(0, 0, 24, SIDEBAR.height);
        let mut decoration = DecorationSnapshot::default();
        decoration.attention.observe_agent(
            "reviewer",
            Some("%0"),
            cyclops_proto::AgentState::BlockedPermission,
        );
        assert!(decoration.workspace_needs_attention());

        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
        let mut term = Terminal::new(TestBackend::new(60, SIDEBAR.height)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_sidebar(
                &workspaces,
                0,
                "%0",
                &std::collections::HashSet::from(["$0".to_string()]),
                &[],
                SidebarTab::Sessions,
                &record,
                narrow,
                f.buffer_mut(),
                &paint,
                &mut hits,
                &decoration,
                None,
                None,
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();

        assert!(matches!(
            hits.hit(1, 0),
            Some(HitTarget::SidebarTab {
                tab: SidebarTab::Sessions
            })
        ));
        assert!(matches!(
            hits.hit(13, 0),
            Some(HitTarget::SidebarTab {
                tab: SidebarTab::Stream
            })
        ));
        let header: String = (0..narrow.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            header.contains('◉'),
            "the rollup has to survive the default width: {header:?}"
        );
    }

    #[test]
    fn the_tab_header_selects_and_hit_tests_its_chips() {
        let record = Record::new();
        let paint = Paint::for_test();

        // " Sessions " is 10 wide from column 1; " Stream " follows flush.
        let (sessions_buf, hits) = draw_sidebar(SidebarTab::Sessions, &record, &paint);
        assert!(matches!(
            hits.hit(1, 0),
            Some(HitTarget::SidebarTab {
                tab: SidebarTab::Sessions
            })
        ));
        assert!(matches!(
            hits.hit(13, 0),
            Some(HitTarget::SidebarTab {
                tab: SidebarTab::Stream
            })
        ));

        let (stream_buf, _) = draw_sidebar(SidebarTab::Stream, &record, &paint);
        let (sessions_chip, stream_chip) = ((2, 0), (14, 0));
        assert_ne!(
            sessions_buf[sessions_chip].bg, sessions_buf[stream_chip].bg,
            "the selected chip needs a materially stronger fill"
        );
        assert_ne!(
            sessions_buf[sessions_chip].bg, stream_buf[sessions_chip].bg,
            "selecting the other tab must repaint the chips"
        );
        assert_eq!(
            sessions_buf[sessions_chip].bg, stream_buf[stream_chip].bg,
            "the selected chip wears one style whichever tab it is"
        );

        // Color off: the fill is gone, so the selection rides on REVERSED.
        let plain = Paint::without_color_for_test();
        let (plain_buf, _) = draw_sidebar(SidebarTab::Stream, &record, &plain);
        assert!(
            plain_buf[stream_chip]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "NO_COLOR must still say which tab is selected"
        );
        assert!(!plain_buf[sessions_chip]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED));
    }

    /// Too narrow for both full chips (`draw_sidebar`'s 42-column fixture is
    /// why the test above never exercises this path): the row splits its
    /// width between the two instead of letting the second one clip off the
    /// edge, and each label ellipsizes inside its own share. Both chips
    /// stay clickable, in their original left-to-right order, and the row
    /// reads as shortened labels rather than a word chopped in half.
    #[test]
    fn a_narrow_tab_header_splits_between_both_chips_and_ellipsizes_their_labels() {
        let workspaces = vec![WorkspaceRow {
            session_id: "$0".into(),
            name: "cyclops".into(),
            tab_count: 1,
            window_ids: vec!["@0".into()],
        }];
        let expanded = std::collections::HashSet::from(["$0".to_string()]);
        let paint = Paint::for_test();
        let width = 12;
        let mut term = Terminal::new(TestBackend::new(width, 8)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| {
            paint_sidebar(
                &workspaces,
                0,
                "%0",
                &expanded,
                &[],
                SidebarTab::Sessions,
                &Record::new(),
                f.area(),
                f.buffer_mut(),
                &paint,
                &mut hits,
                &DecorationSnapshot::default(),
                None,
                None,
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();

        let sessions_chip = (0..width)
            .find(|&x| {
                matches!(
                    hits.hit(x, 0),
                    Some(HitTarget::SidebarTab {
                        tab: SidebarTab::Sessions
                    })
                )
            })
            .expect("the Workspaces chip is still clickable at 12 columns");
        let stream_chip = (0..width)
            .find(|&x| {
                matches!(
                    hits.hit(x, 0),
                    Some(HitTarget::SidebarTab {
                        tab: SidebarTab::Stream
                    })
                )
            })
            .expect("the Stream chip is still clickable at 12 columns");
        assert!(
            stream_chip > sessions_chip,
            "the chips keep their left-to-right order: sessions {sessions_chip}, stream {stream_chip}"
        );

        let header: String = (0..width).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(
            header.contains('…'),
            "a header too narrow for both full labels must ellipsize rather than clip: {header:?}"
        );
    }

    // -- The collapse chevron, in both of its states. --

    /// A theme unlike the default on the one token the chevron paints
    /// through, so a color match against the default theme would mean the
    /// glyph check below was vacuous. The shared `alt_test_theme_paint`
    /// moves state and eye colors, which this control never reads.
    fn accent_test_paint() -> Paint {
        let (theme, warnings) = cyclops_theme::Theme::parse(
            "name = \"accent-test\"\n\
             [surface]\n\
             accent = \"#123456\"\n",
            "accent-test",
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

    /// Paint the rail on its own and hand back the buffer plus the hit
    /// map, the way `app::draw` calls it when prefs say collapsed.
    fn draw_rail(paint: &Paint, hover: Option<(u16, u16)>) -> (Buffer, HitMap) {
        let rail = Rect::new(0, 0, 1, SIDEBAR.height);
        let mut term = Terminal::new(TestBackend::new(20, SIDEBAR.height)).unwrap();
        let mut hits = HitMap::default();
        term.draw(|f| paint_sidebar_rail(rail, f.buffer_mut(), paint, &mut hits, hover))
            .unwrap();
        (term.backend().buffer().clone(), hits)
    }

    /// The whole point of the rail: collapsing must not strand the mouse.
    /// Every row of the one column answers as the toggle, that toggle
    /// routes to the action the chord runs, and the chevron points the way
    /// the click will move the panel.
    #[test]
    fn the_collapsed_rail_answers_the_mouse_and_reopens_the_sidebar() {
        use crossterm::event::MouseButton;

        let paint = Paint::for_test();
        let (buf, hits) = draw_rail(&paint, None);

        for y in 0..SIDEBAR.height {
            assert!(
                matches!(hits.hit(0, y), Some(HitTarget::SidebarToggle)),
                "row {y} of the rail must be clickable"
            );
        }
        assert_eq!(
            crate::action::route_mouse_click(&HitTarget::SidebarToggle, MouseButton::Left),
            Some(crate::action::Action::ToggleSidebar),
            "the chevron is the mouse's half of Ctrl+B b"
        );
        assert_eq!(
            buf[(0, SIDEBAR.height / 2)].symbol(),
            SIDEBAR_EXPAND,
            "collapsed, the chevron points the way the panel will come back"
        );
        // Nothing spills into the canvas: the rail is one column wide.
        for y in 0..SIDEBAR.height {
            assert_eq!(buf[(1, y)].symbol(), " ", "the rail painted past column 0");
        }
    }

    /// The open panel carries the same control on the middle of its own
    /// outer edge, pointing the other way. The row is the one the collapsed
    /// rail uses, so the eye keeps its place across a collapse, and that
    /// equality is the point of the test rather than any absolute row.
    #[test]
    fn the_open_sidebar_carries_the_collapse_chevron_on_its_own_edge() {
        let paint = Paint::for_test();
        let (panel, panel_hits) = draw_sidebar(SidebarTab::Sessions, &Record::new(), &paint);
        let edge = (
            SIDEBAR.x + SIDEBAR.width - 1,
            SIDEBAR.y + SIDEBAR.height / 2,
        );

        assert!(matches!(
            panel_hits.hit(edge.0, edge.1),
            Some(HitTarget::SidebarToggle)
        ));
        // Both states center on the full edge, so the rail's chevron row
        // and the panel's are the same row of the screen.
        let (rail, _) = draw_rail(&paint, None);
        assert_eq!(
            rail[(0, SIDEBAR.height / 2)].symbol(),
            SIDEBAR_EXPAND,
            "the rail puts its chevron on the row the panel just used"
        );
        assert_eq!(
            panel[edge].symbol(),
            SIDEBAR_COLLAPSE,
            "open, the chevron points the way the panel will go"
        );
        assert_ne!(
            SIDEBAR_COLLAPSE, SIDEBAR_EXPAND,
            "the two states must not paint the same arrow"
        );
    }

    /// The open panel's edge is shared with the resize divider, so the
    /// toggle may not own all of it. A band answers, the rest still drags,
    /// and the band is big enough to hit without aiming.
    #[test]
    fn the_collapse_control_leaves_most_of_the_resize_divider_alone() {
        let paint = Paint::for_test();
        let (_, hits) = draw_sidebar(SidebarTab::Sessions, &Record::new(), &paint);
        let col = SIDEBAR.x + SIDEBAR.width - 1;

        let toggle_rows: Vec<u16> = (SIDEBAR.y..SIDEBAR.y + SIDEBAR.height)
            .filter(|y| matches!(hits.hit(col, *y), Some(HitTarget::SidebarToggle)))
            .collect();

        assert!(
            toggle_rows.len() > 1,
            "a one-cell button on a {}-row edge is the thing this replaced",
            SIDEBAR.height
        );
        assert!(
            toggle_rows.len() < usize::from(SIDEBAR.height),
            "the toggle swallowed the resize divider"
        );
        assert!(
            toggle_rows.contains(&(SIDEBAR.y + SIDEBAR.height / 2)),
            "the painted chevron must be inside the band that answers"
        );
    }

    /// The chevron is a control, so it lights under the mouse the way the
    /// create buttons do, and it must not move while being pointed at.
    #[test]
    fn the_chevron_lights_under_the_mouse_without_moving() {
        let paint = Paint::for_test();
        let cell = (0, SIDEBAR.height / 2);
        let (rest, rest_hits) = draw_rail(&paint, None);
        let (hot, hot_hits) = draw_rail(&paint, Some(cell));

        assert_eq!(
            hot_hits.hit(cell.0, cell.1).cloned(),
            rest_hits.hit(cell.0, cell.1).cloned(),
            "the button must not move out from under the mouse that found it"
        );
        assert_eq!(hot[cell].symbol(), rest[cell].symbol(), "same glyph");
        assert_ne!(
            hot[cell].style(),
            rest[cell].style(),
            "pointing at the chevron must change how it paints"
        );
    }

    /// Rule 11: the chevron is chosen by state, never by theme. Two
    /// materially different themes and `NO_COLOR` all paint the same
    /// glyph; only the `Style` under it may move, and it must, or the
    /// glyph check proves nothing.
    #[test]
    fn the_chevron_glyph_is_stable_across_theme_and_no_color() {
        let cell = (0, SIDEBAR.height / 2);
        let (default_buf, _) = draw_rail(&Paint::for_test(), None);
        let (alt_buf, _) = draw_rail(&accent_test_paint(), None);
        let (plain_buf, _) = draw_rail(&Paint::without_color_for_test(), None);

        for buf in [&default_buf, &alt_buf, &plain_buf] {
            assert_eq!(buf[cell].symbol(), SIDEBAR_EXPAND);
        }
        assert_ne!(
            default_buf[cell].fg, alt_buf[cell].fg,
            "the theme change must actually repaint the chevron"
        );
        assert_eq!(
            plain_buf[cell].fg,
            RtColor::Reset,
            "NO_COLOR must leave no color behind, so the glyph is the encoding"
        );
        assert!(
            plain_buf[cell]
                .modifier
                .contains(ratatui::style::Modifier::BOLD),
            "and the control still reads as a control with color off"
        );
    }

    /// The panel used to close itself with a full-height rule one column
    /// from the pane canvas's own border: two parallel lines with nothing
    /// between them, which reads as two windows parked side by side. The
    /// panel's ground meets the canvas now. The column is still the resize
    /// divider, which is the next test's half of the claim.
    #[test]
    fn the_sidebar_edge_paints_panel_ground_not_a_border() {
        let paint = Paint::for_test();
        let (buf, _) = draw_sidebar(SidebarTab::Sessions, &Record::new(), &paint);
        let edge_x = SIDEBAR.x + SIDEBAR.width - 1;
        // A cell no widget writes to: the blank row between the tab header
        // and the tree, inside the sidebar's left pad.
        let ground = buf[(SIDEBAR.x + 1, SIDEBAR.y + 1)].clone();
        assert_eq!(
            Some(ground.bg),
            theme::chrome_panel(&paint).bg,
            "the reference cell must actually be the panel's ground"
        );

        let text = sidebar_text(&buf);
        assert!(
            !text.contains('│'),
            "no vertical rule anywhere in the panel: {text:?}"
        );
        for y in SIDEBAR.y..SIDEBAR.y + SIDEBAR.height {
            let cell = &buf[(edge_x, y)];
            // The collapse control is meant to be on this edge, and it
            // paints itself. Every other row is bare ground.
            if cell.symbol() == SIDEBAR_COLLAPSE {
                continue;
            }
            assert_eq!(
                cell.symbol(),
                " ",
                "row {y} of the sidebar's outer column still paints something"
            );
            assert_eq!(
                (cell.fg, cell.bg),
                (ground.fg, ground.bg),
                "row {y} of the outer column must be the panel's own ground"
            );
        }
    }

    /// Resize by drag survives the header and the collapse chevron. The
    /// sidebar's rightmost column is the `SidebarDivider` the drag reads
    /// (`app::handle_mouse`) on every row of both tabs except the footer
    /// row, which the chevron claims. A chip that clipped into that
    /// column, or a header that shortened the divider, would take the
    /// handle away.
    #[test]
    fn the_tab_header_leaves_the_resize_divider_on_every_row_but_the_chevrons() {
        let record = one_row_record();
        let paint = Paint::for_test();
        let divider_x = SIDEBAR.x + SIDEBAR.width - 1;
        // The chevron owns a centered band of the edge now, not the footer
        // cell. Derive it from the same function the painter uses so the
        // test tracks the control instead of restating its arithmetic.
        let band = toggle_reach(Rect::new(divider_x, SIDEBAR.y, 1, SIDEBAR.height));
        for tab in [SidebarTab::Sessions, SidebarTab::Stream] {
            let (_, hits) = draw_sidebar(tab, &record, &paint);
            for y in SIDEBAR.y..SIDEBAR.y + SIDEBAR.height {
                let in_band = y >= band.y && y < band.y + band.height;
                if in_band {
                    assert!(
                        matches!(hits.hit(divider_x, y), Some(HitTarget::SidebarToggle)),
                        "{tab:?}: row {y} is in the chevron band and must toggle"
                    );
                } else {
                    assert!(
                        matches!(hits.hit(divider_x, y), Some(HitTarget::SidebarDivider)),
                        "{tab:?}: row {y} lost the resize handle"
                    );
                }
            }
        }
    }
}
