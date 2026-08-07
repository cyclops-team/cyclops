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
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

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
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(theme::pane_border(paint))
        .style(theme::chrome_panel(paint));
    let inner = block.inner(area);
    block.render(area, buf);
    hits.push(
        Rect::new(
            area.x + area.width.saturating_sub(1),
            area.y,
            1,
            area.height,
        ),
        HitTarget::SidebarDivider,
    );

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
    match tab {
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
        }
    }
    paint_footer(inner, content, footer_y, buf, paint, hits, hover);
    // The collapse control sits on the panel's outer edge, on the footer
    // row. Same row as the rail's chevron, so the control stays where the
    // eye left it when the panel goes; the column moves because the panel
    // it was attached to did. It takes one cell of the resize divider and
    // no more: the handle still answers on every other row, and this hit
    // is pushed last so it wins its own.
    paint_toggle(
        buf,
        Rect::new(area.x + area.width.saturating_sub(1), footer_y, 1, 1),
        SIDEBAR_COLLAPSE,
        paint,
        hits,
        hover,
    );
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
    paint_toggle(buf, area, SIDEBAR_EXPAND, paint, hits, hover);
}

/// Paint one sidebar chevron at the foot of `hit` and register that whole
/// rectangle as the toggle. The glyph lands on the rectangle's bottom-left
/// cell; the button fills under the mouse exactly the way the create
/// buttons do, so every chrome control in this workspace answers a pointer
/// the same way.
fn paint_toggle(
    buf: &mut Buffer,
    hit: Rect,
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
    // The whole strip is clickable, so the whole strip lights: feedback
    // has to appear under the pointer, and a rail is thirty rows tall
    // with its glyph at the foot. Lighting the glyph alone answered a
    // hover the operator made twenty-nine rows away.
    if hovered {
        for row in hit.y..hit.y + hit.height {
            if let Some(cell) = buf.cell_mut((hit.x, row)) {
                cell.set_style(style);
            }
        }
    }
    let y = hit.y + hit.height - 1;
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
    let mut spans = vec![Span::styled(" ", theme::chrome_panel(paint))];
    let mut x = inner.x + 1.min(inner.width);
    let right = inner.x + inner.width;
    // Chips sit flush against each other: at the 22-column minimum both
    // labels plus the rollup fill the row exactly, and a narrower sidebar
    // clips from the right rather than dropping a chip.
    for (chip, label) in [
        (SidebarTab::Sessions, copy::SIDEBAR_TAB_SESSIONS),
        (SidebarTab::Stream, copy::SIDEBAR_TAB_STREAM),
    ] {
        let style = if chip == tab {
            theme::tab_active(paint)
        } else {
            theme::tab_inactive(paint)
        };
        let text = format!(" {label} ");
        let w = u16::try_from(Span::raw(text.as_str()).width()).unwrap_or(u16::MAX);
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
            theme::attention_eye(paint).patch(paint.bg_token(cyclops_theme::tokens::CHROME_PANEL)),
        ));
    }
    Paragraph::new(Line::from(spans)).render(row, buf);
}

/// The Sessions tab's body: workspace rows, their expanded agent rows, and
/// the live reorder-drop rule.
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
) {
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
    if !decoration.online {
        super::overlay_text(
            buf,
            content,
            content.x,
            y,
            "cyclopsd offline",
            theme::sidebar_row(paint),
        );
        y += 1;
    }
    for (i, ws) in workspaces.iter().enumerate() {
        if y >= footer_y {
            break;
        }
        let expanded = expanded_workspaces.contains(&ws.session_id);
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
        super::overlay_text(
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

        if !expanded {
            continue;
        }
        for agent in decoration.agent_rows_for_window_ids(&ws.window_ids, agent_order) {
            if y >= footer_y {
                break;
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
            super::overlay_text(buf, content, x, y, name, name_style);
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
}

/// The footer both tabs share: the application menu at left, and a matching
/// compact create button anchoring the hierarchy at bottom-right without
/// stealing the rest of the row.
///
/// Painted for the Stream tab too. The menu button is the only mouse route
/// to themes, keybinds, and detach, so hiding it behind a tab would strand
/// them.
fn paint_footer(
    inner: Rect,
    content: Rect,
    footer_y: u16,
    buf: &mut Buffer,
    paint: &Paint,
    hits: &mut HitMap,
    hover: Option<(u16, u16)>,
) {
    if inner.height < 2 {
        return;
    }
    let menu_width = u16::try_from(Span::raw(copy::APP_MENU_BUTTON).width())
        .unwrap_or(u16::MAX)
        .min(content.width);
    super::overlay_text(
        buf,
        content,
        content.x,
        footer_y,
        copy::APP_MENU_BUTTON,
        theme::sidebar_label(paint),
    );
    hits.push(
        Rect::new(content.x, footer_y, menu_width, 1),
        HitTarget::AppMenu,
    );
    let plus = " + ";
    let plus_width = u16::try_from(Span::raw(plus).width())
        .unwrap_or(u16::MAX)
        .min(content.width);
    let plus_x = content
        .x
        .saturating_add(content.width.saturating_sub(plus_width));
    // The button keeps one width whether or not it is pointed at, so
    // the target never moves out from under the mouse that found it.
    let hovered = hover.is_some_and(|(hover_col, hover_row)| {
        hover_row == footer_y
            && hover_col >= plus_x
            && hover_col < plus_x.saturating_add(plus_width)
    });
    if hovered {
        // Say what it makes, in the gutter the footer already leaves
        // between the menu label and the button. Skipped rather than
        // truncated when the sidebar is too narrow: half a word next to
        // a lit button teaches nothing.
        let hint_width =
            u16::try_from(Span::raw(copy::NEW_WORKSPACE_HINT).width()).unwrap_or(u16::MAX);
        let gutter = plus_x.saturating_sub(content.x.saturating_add(menu_width));
        if hint_width < gutter {
            super::overlay_text(
                buf,
                content,
                plus_x.saturating_sub(hint_width),
                footer_y,
                copy::NEW_WORKSPACE_HINT,
                theme::sidebar_label(paint),
            );
        }
    }
    super::overlay_text(
        buf,
        content,
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
        // The title row is followed by a spacer, then the offline note,
        // so workspaces paint on rows 3 and 4.
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
        // Bottom row carries distinct menu and create buttons.
        assert!(matches!(hits.hit(2, 7), Some(HitTarget::AppMenu)));
        assert!(matches!(
            hits.hit(15, 7),
            Some(HitTarget::NewWorkspaceButton)
        ));
        assert!(flat.contains("menu"), "menu button should render: {flat}");
        assert!(flat.contains('+'), "create button should render: {flat}");
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
        let inner_width = 19; // area width 20 minus the 1-cell right border
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
            "the rule must not paint over the sidebar's own border column"
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
        assert!(
            matches!(
                hits.hit(6, 3),
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

        // Column 5, row 3: one expanded, online workspace puts its first
        // agent row at y = 3 (title, blank, workspace row), and the status
        // glyph lands 3 cells past the 2-cell sidebar pad
        // (`content.x.saturating_add(3)` in `paint_sidebar`).
        let (gx, gy) = (5, 3);
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
        let narrow = Rect::new(0, 0, 22, SIDEBAR.height);
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
            hits.hit(11, 0),
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
            hits.hit(11, 0),
            Some(HitTarget::SidebarTab {
                tab: SidebarTab::Stream
            })
        ));

        let (stream_buf, _) = draw_sidebar(SidebarTab::Stream, &record, &paint);
        let (sessions_chip, stream_chip) = ((2, 0), (12, 0));
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
            buf[(0, SIDEBAR.height - 1)].symbol(),
            SIDEBAR_EXPAND,
            "collapsed, the chevron points the way the panel will come back"
        );
        // Nothing spills into the canvas: the rail is one column wide.
        for y in 0..SIDEBAR.height {
            assert_eq!(buf[(1, y)].symbol(), " ", "the rail painted past column 0");
        }
    }

    /// The open panel carries the same control on the same row of its own
    /// outer edge, pointing the other way. Same row as the rail's, so the
    /// eye keeps its place across a collapse.
    #[test]
    fn the_open_sidebar_carries_the_collapse_chevron_on_its_own_edge() {
        let paint = Paint::for_test();
        let (panel, panel_hits) = draw_sidebar(SidebarTab::Sessions, &Record::new(), &paint);
        let edge = (
            SIDEBAR.x + SIDEBAR.width - 1,
            SIDEBAR.y + SIDEBAR.height - 1,
        );

        assert!(matches!(
            panel_hits.hit(edge.0, edge.1),
            Some(HitTarget::SidebarToggle)
        ));
        assert_eq!(
            edge.1,
            SIDEBAR.y + SIDEBAR.height - 1,
            "the same row the collapsed rail puts its chevron on"
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

    /// The chevron is a control, so it lights under the mouse the way the
    /// create buttons do, and it must not move while being pointed at.
    #[test]
    fn the_chevron_lights_under_the_mouse_without_moving() {
        let paint = Paint::for_test();
        let cell = (0, SIDEBAR.height - 1);
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
        let cell = (0, SIDEBAR.height - 1);
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
        let chevron_y = SIDEBAR.y + SIDEBAR.height - 1;
        for tab in [SidebarTab::Sessions, SidebarTab::Stream] {
            let (_, hits) = draw_sidebar(tab, &record, &paint);
            for y in SIDEBAR.y..chevron_y {
                assert!(
                    matches!(hits.hit(divider_x, y), Some(HitTarget::SidebarDivider)),
                    "{tab:?}: row {y} lost the resize handle"
                );
            }
            assert!(
                matches!(
                    hits.hit(divider_x, chevron_y),
                    Some(HitTarget::SidebarToggle)
                ),
                "{tab:?}: the collapse chevron owns the footer row of the edge"
            );
        }
    }
}
