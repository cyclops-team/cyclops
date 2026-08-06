//! The workspace sidebar: session rows, their expanded agent rows, the
//! live reorder-drop rule, and the app-menu/create-workspace footer.
//! Reorder math itself (which slot a drag previews) belongs to
//! `crate::drag`; this only paints whatever slot it is told.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Widget};

use crate::copy;
use crate::decoration::DecorationSnapshot;
use crate::drag::{DragState, DragTarget};
use crate::input::mouse::{HitMap, HitTarget};
use crate::model::WorkspaceRow;
use crate::theme::{self, Paint};

/// Render the workspace sidebar.
pub fn paint_sidebar(
    workspaces: &[WorkspaceRow],
    active: usize,
    active_pane: &str,
    expanded_workspaces: &std::collections::HashSet<String>,
    agent_order: &[String],
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
    // A live workspace-row drag: which row is grabbed (dimmed in the loop
    // below) and, once the pointer is actually over this sidebar, which
    // slot it currently previews.
    let dragging_session = drag
        .filter(|d| d.is_active())
        .and_then(|d| match &d.target {
            DragTarget::Workspace { session_id, .. } => Some(session_id.as_str()),
            _ => None,
        });
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
    let eye = if decoration.workspace_needs_attention() {
        " ◉"
    } else {
        ""
    };
    super::overlay_text(
        buf,
        content,
        content.x,
        content.y,
        "Workspaces",
        theme::sidebar_label(paint).add_modifier(Modifier::BOLD),
    );
    super::overlay_text(
        buf,
        content,
        content.x + "Workspaces".len() as u16,
        content.y,
        eye,
        theme::attention_eye(paint).patch(paint.bg_token(cyclops_theme::tokens::CHROME_PANEL)),
    );
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
    let footer_y = inner.y + inner.height.saturating_sub(1);
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
            &format!("{grip}{marker} {} ({})", ws.name, ws.tab_count),
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

    // Application menu at left; a matching compact create button anchors the
    // hierarchy at bottom-right without stealing the rest of the footer row.
    if inner.height >= 2 {
        let menu_row = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        let menu_width = u16::try_from(Span::raw(copy::APP_MENU_BUTTON).width())
            .unwrap_or(u16::MAX)
            .min(content.width);
        super::overlay_text(
            buf,
            content,
            content.x,
            menu_row.y,
            copy::APP_MENU_BUTTON,
            theme::sidebar_label(paint),
        );
        hits.push(
            Rect::new(content.x, menu_row.y, menu_width, 1),
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
            hover_row == menu_row.y
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
                    menu_row.y,
                    copy::NEW_WORKSPACE_HINT,
                    theme::sidebar_label(paint),
                );
            }
        }
        super::overlay_text(
            buf,
            content,
            plus_x,
            menu_row.y,
            plus,
            if hovered {
                theme::add_button_hover(paint)
            } else {
                theme::add_button(paint)
            },
        );
        hits.push(
            Rect::new(plus_x, menu_row.y, plus_width, 1),
            HitTarget::NewWorkspaceButton,
        );
    }
}

/// The workspace-reorder drop indicator: a full-width accent rule at row
/// `y`, spanning `area`'s usable width. Called only while a workspace-row
/// drag is live and the pointer sits over the sidebar — see the call site
/// in [`paint_sidebar`].
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
}
