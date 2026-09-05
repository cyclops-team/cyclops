//! Mouse hit regions and routing.

use ratatui::layout::Rect;

use crate::bindings::BindingAction;
use crate::layout::SplitDir;

/// What a click or wheel event targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitTarget {
    PaneBody {
        pane_id: String,
    },
    PaneFrame {
        pane_id: String,
    },
    /// The one-cell swap handle in the frame's bottom-right corner. The
    /// only frame cell that picks a pane up; every other frame cell is a
    /// focus click, so the resize seams a frame shares with its siblings
    /// stay resize handles.
    PaneGrip {
        pane_id: String,
    },
    PaneSplitRight {
        pane_id: String,
    },
    PaneSplitDown {
        pane_id: String,
    },
    PaneRename {
        pane_id: String,
    },
    Divider {
        pane_id: String,
        dir: SplitDir,
    },
    Tab {
        window_id: String,
    },
    NewTabButton,
    /// The tab strip's chat button: opens the composer, the mouse's half of
    /// Ctrl+B @.
    ComposeButton,
    SidebarRow {
        session_id: String,
        session: String,
    },
    SidebarDisclosure {
        session_id: String,
    },
    SidebarAgent {
        workspace_id: String,
        pane_id: String,
        order_key: String,
    },
    /// One chip of the sidebar's Sessions/Stream tab header.
    SidebarTab {
        tab: crate::persist::SidebarTab,
    },
    /// One chip of the sidebar's session tree filter header.
    SidebarFilter {
        filter: crate::persist::SidebarFilter,
    },
    SidebarDivider,
    /// One entry of the file panel. Carries what a click needs so the
    /// handler never re-reads the filesystem to answer a press: `path` to
    /// walk into for a folder, `reference` (root-relative) to send for a
    /// file.
    FileRow {
        index: usize,
        path: String,
        is_dir: bool,
        reference: String,
    },
    /// A folder row's chevron column only. Opens or closes that folder in
    /// place, where a click on the rest of the row walks into it.
    ///
    /// Two targets on one row, the same shape as [`Self::SidebarRow`] and
    /// [`Self::SidebarDisclosure`] on a workspace row. Both sit on the
    /// same [`HitLayer`], where the last region pushed over a cell is the
    /// one that answers there, so the narrow one is pushed second.
    FileDisclosure {
        path: String,
    },
    /// A pane's minimize control, at the left end of its top border.
    /// Collapses the pane to its title, and restores it.
    PaneMinimize {
        pane_id: String,
    },
    /// The file panel's climb-out row.
    FileUp,
    /// The file panel's header: re-roots on the focused pane's folder
    /// (agent view) or the saved pinned folder (pinned view).
    FileRoot,
    /// The header chip that flips the panel between the agent-following
    /// browser and the pinned one.
    FilesViewToggle,
    /// Retrace one step of the walk, and undo that. Only pushed while the
    /// step exists, so neither is ever a control that answers with nothing.
    FileBack,
    FileForward,
    /// The seam between the session tree and the file panel.
    SidebarSplit,
    /// The chevron that collapses or reopens the sidebar: on the open
    /// panel's outer edge, and on the one-column rail a collapse leaves
    /// behind. One target for both, because it is one control.
    SidebarToggle,
    /// The chevron that collapses or reopens the right-edge Messages pane:
    /// on the open panel's inner divider edge, and on the one-column rail
    /// a collapse leaves behind.
    MessagesToggle,
    /// Resize handle for the right-edge Messages pane.
    MessagesDivider,
    /// One verb in the Messages pane's action strip. The strip prints the same
    /// words a keyboard user reads, so a pointer gets the same verbs
    /// instead of a hint it cannot act on.
    MessagesAction(cyclops_ui::ChatAction),
    AttentionIndicator {
        pane_id: String,
    },
    /// A clear / backspace button on the pane frame to unblock or wipe a composer draft.
    PaneClear {
        pane_id: String,
    },
    AppMenu,
    NewWorkspaceButton,
    MenuItem {
        action: BindingAction,
    },
    DialogConfirm,
    DialogCancel,
    /// The top border and title row of an open dialog: the rows that move
    /// the box rather than answer it.
    DialogTitleBar,
    /// One section chip of the settings dialog: the mouse's half of Tab.
    SettingsSection {
        section: crate::dialog::SettingsSection,
    },
    /// One row of the settings dialog's showing list: the mouse's half of
    /// the arrows. A click puts the cursor on it (and previews a theme);
    /// applying is still `Enter` or the button.
    SettingsRow {
        index: usize,
    },
}

/// Geometry recorded during render for cell hit-testing.
#[derive(Debug, Clone)]
pub struct PaneGeometry {
    pub pane_id: String,
    pub inner: Rect,
}

/// Which stratum of the frame a hit region belongs to, lowest first.
///
/// A click lands on the highest layer painted over its cell, whatever
/// order the painters ran in. Before this the map answered with the
/// last region pushed, and every painter had to know what every other
/// painter had already pushed: the sidebar divider re-pushed itself over
/// the pane frames, the clear button re-pushed itself over the sidebar
/// divider, and each of those was a comment away from breaking.
///
/// The order encodes what a pointer expects. A resize seam beats the
/// passive border and body it is drawn across, because a border nobody
/// can grab is a pane that only resizes from its far edge. A pane's
/// visible buttons beat the seam, because a control that shows an `X`
/// and then starts a resize is worse than no control at all. Panels
/// and their chrome sit above the canvas they border, and a menu or
/// dialog floats over everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HitLayer {
    /// Pane bodies and their passive frame edges: focus clicks and text
    /// selection.
    Canvas,
    /// Resize handles: the bands between sibling panes, the sidebar's
    /// and the Messages pane's outer columns, the seam between the
    /// sidebar's two panels.
    Seam,
    /// The controls painted over a pane's frame: title strip, minimize,
    /// clear, grip, rename and split buttons.
    PaneChrome,
    /// The sidebar, tab strip and Messages pane: rows, chips, toggles,
    /// footer buttons.
    SidebarChrome,
    /// Menus and dialogs.
    Overlay,
}

/// One recorded hit region from the last render pass.
#[derive(Debug, Clone)]
pub struct HitRegion {
    pub rect: Rect,
    pub target: HitTarget,
    pub layer: HitLayer,
}

impl HitRegion {
    fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.rect.x
            && col < self.rect.x + self.rect.width
            && row >= self.rect.y
            && row < self.rect.y + self.rect.height
    }
}

/// Regions painted during the last frame, tested on mouse events.
#[derive(Default)]
pub struct HitMap {
    regions: Vec<HitRegion>,
    pane_geometries: Vec<PaneGeometry>,
}

impl HitMap {
    pub fn clear(&mut self) {
        self.regions.clear();
        self.pane_geometries.clear();
    }

    /// Drop the overlay's regions as soon as a menu closes. The next
    /// frame restores the complete map; until then a fast second click
    /// must not replay a command through stale overlay geometry.
    pub fn clear_overlay(&mut self) {
        self.regions
            .retain(|region| region.layer < HitLayer::Overlay);
    }

    pub fn push_geometry(&mut self, geometry: PaneGeometry) {
        self.pane_geometries.push(geometry);
    }

    pub fn pane_geometry(&self, pane_id: &str) -> Option<&PaneGeometry> {
        self.pane_geometries.iter().find(|g| g.pane_id == pane_id)
    }

    pub fn push(&mut self, rect: Rect, layer: HitLayer, target: HitTarget) {
        if rect.width > 0 && rect.height > 0 {
            self.regions.push(HitRegion {
                rect,
                target,
                layer,
            });
        }
    }

    /// The sidebar's workspace rows as contiguous vertical blocks, built
    /// from the rects the last frame actually painted rather than
    /// recomputed independently — see [`crate::drag::WorkspaceBlock`]. Each
    /// `SidebarRow` starts a new block; any `SidebarAgent` rows pushed
    /// right after it (only present while that workspace is expanded) fold
    /// into the SAME block, so the boundary math built on top of this never
    /// lands inside one's agent rows. A workspace the sidebar clipped for
    /// lack of height never got a rect pushed, so it is simply absent here.
    pub fn workspace_blocks(&self) -> Vec<crate::drag::WorkspaceBlock> {
        let mut blocks: Vec<crate::drag::WorkspaceBlock> = Vec::new();
        for region in &self.regions {
            match &region.target {
                HitTarget::SidebarRow { session_id, .. } => {
                    blocks.push(crate::drag::WorkspaceBlock {
                        session_id: session_id.clone(),
                        top: region.rect.y,
                        bottom: region.rect.y + region.rect.height,
                    });
                }
                HitTarget::SidebarAgent { .. } => {
                    if let Some(last) = blocks.last_mut() {
                        last.bottom = last.bottom.max(region.rect.y + region.rect.height);
                    }
                }
                _ => {}
            }
        }
        blocks
    }

    /// The resize seam under `col`/`row`, whatever is painted over it.
    ///
    /// Kept beside [`HitMap::hit`] for one overlay: a pane's title strip.
    /// It sits on [`HitLayer::PaneChrome`] because it is a real control (a
    /// focus click, or the attention eye), so `hit` rightly answers with
    /// it. But the row it sits on is also the seam between two stacked
    /// panes, and a seam nobody can grab is a pane that can only be
    /// resized from its far edge. Measured: of the 40 cells on a two-pane
    /// seam's lower row, one was grabbable. `app` asks here after a press
    /// on the strip, and moves the seam if the pointer moves.
    ///
    /// Seams never overlap each other today (an outer divider sits outside
    /// every child's rect), so which of two is answered is only
    /// load-bearing if that ever stops being true; the last pushed wins,
    /// the same rule `hit` applies within a layer.
    pub fn divider_at(&self, col: u16, row: u16) -> Option<(&str, SplitDir)> {
        self.regions
            .iter()
            .rev()
            .filter(|r| r.contains(col, row))
            .find_map(|r| match &r.target {
                HitTarget::Divider { pane_id, dir } => Some((pane_id.as_str(), *dir)),
                _ => None,
            })
    }

    /// The region that answers a pointer at `col`/`row`: the highest
    /// [`HitLayer`] painted there, and within a layer the last pushed.
    pub fn hit(&self, col: u16, row: u16) -> Option<&HitTarget> {
        self.regions
            .iter()
            .filter(|r| r.contains(col, row))
            // `max_by_key` keeps the last of equal maxima, which is the
            // within-layer rule.
            .max_by_key(|r| r.layer)
            .map(|r| &r.target)
    }

    pub fn regions(&self) -> &[HitRegion] {
        &self.regions
    }

    /// Map terminal coordinates to a cell inside a pane body.
    pub fn cell_at(geom: &PaneGeometry, col: u16, row: u16) -> Option<crate::runtime::CellPos> {
        if col < geom.inner.x
            || col >= geom.inner.x + geom.inner.width
            || row < geom.inner.y
            || row >= geom.inner.y + geom.inner.height
        {
            return None;
        }
        Some(crate::runtime::CellPos {
            col: col - geom.inner.x,
            row: row - geom.inner.y,
        })
    }

    /// Map terminal coordinates to a cell inside a pane body, clamping to its
    /// bounds if the coordinates extend outside the pane (e.g. during a drag selection).
    pub fn cell_at_clamped(
        geom: &PaneGeometry,
        col: u16,
        row: u16,
    ) -> Option<crate::runtime::CellPos> {
        if geom.inner.width == 0 || geom.inner.height == 0 {
            return None;
        }
        let max_col = geom.inner.x + geom.inner.width - 1;
        let max_row = geom.inner.y + geom.inner.height - 1;
        Some(crate::runtime::CellPos {
            col: col.clamp(geom.inner.x, max_col) - geom.inner.x,
            row: row.clamp(geom.inner.y, max_row) - geom.inner.y,
        })
    }
}

/// Whether this motion arrives on, or departs from, a chrome button that
/// lights under the mouse. Both edges have to reach the renderer: one
/// lights the button, the other puts it out, and a filter that only let
/// the arrival through would leave it lit wherever the mouse went next.
///
/// One list for every such button, so a control added to the chrome cannot
/// paint a hover state the event filter never delivers.
pub fn motion_touches_hover_button(
    hit_map: &HitMap,
    hover: Option<(u16, u16)>,
    col: u16,
    row: u16,
) -> bool {
    let on_button = |col: u16, row: u16| {
        matches!(
            hit_map.hit(col, row),
            // The controls rule 1 in `render/mod.rs` names: the tab strip's
            // `+` and `@`, the sidebar footer's `+`, and the sidebar
            // chevron. `NewTabButton` was absent while `paint_tab_bar`
            // already computed a hover for it, so that one button saw no
            // motion event and never lit.
            //
            // The sidebar's resize handle is on the list for the same
            // reason: it paints nothing at rest and reveals itself under
            // the pointer, so it has no state to show at all without the
            // motion that arrives here.
            // Every file panel target, not just its buttons. The panel
            // lights a row under the pointer, and a row is only a hover
            // state like any other: nothing else redraws for it, so a row
            // left off this list simply never lights. The rest of this
            // list is the same lesson learned one control at a time.
            Some(
                HitTarget::NewTabButton
                    | HitTarget::ComposeButton
                    | HitTarget::NewWorkspaceButton
                    | HitTarget::SidebarToggle
                    | HitTarget::SidebarDivider
                    | HitTarget::MessagesToggle
                    | HitTarget::MessagesDivider
                    // The Messages pane's footer buttons fill under the pointer
                    // the way the tab strip's `+` does.
                    | HitTarget::MessagesAction(_)
                    | HitTarget::FileRow { .. }
                    | HitTarget::FileDisclosure { .. }
                    | HitTarget::FileUp
                    | HitTarget::FileRoot
                    | HitTarget::FilesViewToggle
                    | HitTarget::FileBack
                    | HitTarget::FileForward
            )
        )
    };
    on_button(col, row) || hover.is_some_and(|(col, row)| on_button(col, row))
}

/// Open menu state — at most one menu is open at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuState {
    None,
    AppMenu,
    ContextMenu { pane_id: String, at: (u16, u16) },
    TabMenu { window_id: String, at: (u16, u16) },
    WorkspaceMenu { session_id: String, at: (u16, u16) },
}

impl MenuState {
    pub fn close(&mut self) {
        *self = MenuState::None;
    }

    pub fn is_open(&self) -> bool {
        *self != MenuState::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(window_id: &str) -> HitTarget {
        HitTarget::Tab {
            window_id: window_id.into(),
        }
    }

    fn body(pane_id: &str) -> HitTarget {
        HitTarget::PaneBody {
            pane_id: pane_id.into(),
        }
    }

    #[test]
    fn hit_test_returns_the_last_pushed_within_one_layer() {
        let mut map = HitMap::default();
        map.push(Rect::new(0, 0, 10, 10), HitLayer::Canvas, tab("@0"));
        map.push(Rect::new(2, 2, 4, 4), HitLayer::Canvas, body("%0"));
        assert!(matches!(map.hit(3, 3), Some(HitTarget::PaneBody { .. })));
        assert!(matches!(
            map.hit(9, 9),
            Some(HitTarget::Tab { window_id }) if window_id == "@0"
        ));
    }

    /// The layer decides, not the push order. The sidebar divider used to
    /// be pushed after the pane frames so it could win its column, and
    /// the clear button pushed again after that so it could win its two
    /// cells back; each was a comment away from breaking.
    #[test]
    fn hit_test_returns_the_highest_layer_whatever_the_push_order() {
        let mut map = HitMap::default();
        // The clear button, then the sidebar divider over its column,
        // then the pane frame under both: the order every painter would
        // have had to agree on, reversed.
        map.push(
            Rect::new(0, 4, 2, 3),
            HitLayer::PaneChrome,
            HitTarget::PaneClear {
                pane_id: "%0".into(),
            },
        );
        map.push(
            Rect::new(0, 0, 1, 12),
            HitLayer::Seam,
            HitTarget::SidebarDivider,
        );
        map.push(
            Rect::new(0, 0, 1, 12),
            HitLayer::Canvas,
            HitTarget::PaneFrame {
                pane_id: "%0".into(),
            },
        );

        assert!(
            matches!(map.hit(0, 5), Some(HitTarget::PaneClear { .. })),
            "the button answers its own cells"
        );
        assert!(
            matches!(map.hit(0, 1), Some(HitTarget::SidebarDivider)),
            "the divider answers the rest of the column"
        );
        // A menu over all of it answers first however early it was pushed.
        map.regions.insert(
            0,
            HitRegion {
                rect: Rect::new(0, 0, 4, 12),
                target: HitTarget::MenuItem {
                    action: BindingAction::ClosePane,
                },
                layer: HitLayer::Overlay,
            },
        );
        assert!(matches!(map.hit(0, 5), Some(HitTarget::MenuItem { .. })));
    }

    /// The title strip is a control and answers `hit`, but the seam it is
    /// painted over must still be found under it for a press to move.
    #[test]
    fn divider_at_finds_the_seam_under_pane_chrome() {
        let mut map = HitMap::default();
        map.push(
            Rect::new(1, 5, 38, 2),
            HitLayer::Seam,
            HitTarget::Divider {
                pane_id: "%0".into(),
                dir: SplitDir::Vertical,
            },
        );
        map.push(
            Rect::new(5, 6, 20, 1),
            HitLayer::PaneChrome,
            HitTarget::PaneFrame {
                pane_id: "%1".into(),
            },
        );
        assert!(matches!(
            map.hit(10, 6),
            Some(HitTarget::PaneFrame { pane_id }) if pane_id == "%1"
        ));
        assert_eq!(
            map.divider_at(10, 6),
            Some(("%0", SplitDir::Vertical)),
            "the seam is still there under the strip"
        );
        assert_eq!(map.divider_at(10, 8), None);
    }

    #[test]
    fn workspace_blocks_folds_agent_rows_into_their_own_workspace() {
        let mut map = HitMap::default();
        // $a is collapsed: one row, one block.
        map.push(
            Rect::new(0, 3, 20, 1),
            HitLayer::SidebarChrome,
            HitTarget::SidebarRow {
                session_id: "$a".into(),
                session: "a".into(),
            },
        );
        // $b is expanded: its header plus two agent rows fold into one
        // block spanning rows 4..7.
        map.push(
            Rect::new(0, 4, 20, 1),
            HitLayer::SidebarChrome,
            HitTarget::SidebarRow {
                session_id: "$b".into(),
                session: "b".into(),
            },
        );
        map.push(
            Rect::new(0, 5, 20, 1),
            HitLayer::SidebarChrome,
            HitTarget::SidebarAgent {
                workspace_id: "$b".into(),
                pane_id: "%1".into(),
                order_key: "pane:%1".into(),
            },
        );
        map.push(
            Rect::new(0, 6, 20, 1),
            HitLayer::SidebarChrome,
            HitTarget::SidebarAgent {
                workspace_id: "$b".into(),
                pane_id: "%2".into(),
                order_key: "pane:%2".into(),
            },
        );
        // A disclosure hit is pushed alongside every row but must not
        // create a block of its own.
        map.push(
            Rect::new(0, 3, 1, 1),
            HitLayer::SidebarChrome,
            HitTarget::SidebarDisclosure {
                session_id: "$a".into(),
            },
        );

        let blocks = map.workspace_blocks();

        assert_eq!(
            blocks,
            vec![
                crate::drag::WorkspaceBlock {
                    session_id: "$a".into(),
                    top: 3,
                    bottom: 4,
                },
                crate::drag::WorkspaceBlock {
                    session_id: "$b".into(),
                    top: 4,
                    bottom: 7,
                },
            ]
        );
    }

    #[test]
    fn workspace_blocks_is_empty_without_any_sidebar_rows() {
        let mut map = HitMap::default();
        map.push(Rect::new(0, 0, 10, 1), HitLayer::SidebarChrome, tab("@0"));
        assert!(map.workspace_blocks().is_empty());
    }

    /// All three hover-lit chrome buttons are on the filter's list, and
    /// both edges of a motion get through: a button that lit on arrival but
    /// never heard the departure would stay lit wherever the mouse went
    /// next.
    ///
    /// The tab strip's `+` is here because it shipped missing: the strip
    /// painted a hover state the filter never delivered an event for.
    #[test]
    fn motion_reaches_the_renderer_for_every_button_that_lights() {
        let mut map = HitMap::default();
        map.push(
            Rect::new(0, 7, 1, 1),
            HitLayer::SidebarChrome,
            HitTarget::SidebarToggle,
        );
        map.push(
            Rect::new(15, 7, 3, 1),
            HitLayer::SidebarChrome,
            HitTarget::NewWorkspaceButton,
        );
        map.push(
            Rect::new(25, 0, 3, 1),
            HitLayer::SidebarChrome,
            HitTarget::NewTabButton,
        );
        map.push(Rect::new(0, 0, 20, 5), HitLayer::Canvas, body("%0"));

        assert!(motion_touches_hover_button(&map, None, 0, 7), "the chevron");
        assert!(
            motion_touches_hover_button(&map, None, 16, 7),
            "the create button"
        );
        assert!(
            motion_touches_hover_button(&map, None, 26, 0),
            "the tab strip's +"
        );
        // Leaving any of them still has to repaint it.
        assert!(motion_touches_hover_button(&map, Some((0, 7)), 3, 3));
        assert!(motion_touches_hover_button(&map, Some((16, 7)), 3, 3));
        assert!(motion_touches_hover_button(&map, Some((26, 0)), 3, 3));
        // Motion with neither end on a button is the noise the filter is
        // for.
        assert!(!motion_touches_hover_button(&map, Some((4, 4)), 3, 3));
    }

    #[test]
    fn closing_a_menu_removes_only_its_stale_rows() {
        let mut map = HitMap::default();
        map.push(Rect::new(0, 0, 10, 5), HitLayer::Canvas, body("%0"));
        map.push(
            Rect::new(1, 1, 8, 1),
            HitLayer::Overlay,
            HitTarget::MenuItem {
                action: BindingAction::ClosePane,
            },
        );

        map.clear_overlay();

        assert!(matches!(map.hit(2, 1), Some(HitTarget::PaneBody { .. })));
    }
}
