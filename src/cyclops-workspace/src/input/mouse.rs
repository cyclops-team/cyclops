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
    /// [`Self::SidebarDisclosure`] on a workspace row. The narrow one is
    /// pushed second so it wins its own cells: [`HitMap::hit`] scans in
    /// reverse, so the last region pushed over a cell is the one that
    /// answers there.
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
    AttentionIndicator {
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
}

/// Geometry recorded during render for cell hit-testing.
#[derive(Debug, Clone)]
pub struct PaneGeometry {
    pub pane_id: String,
    pub inner: Rect,
}

/// One recorded hit region from the last render pass.
#[derive(Debug, Clone)]
pub struct HitRegion {
    pub rect: Rect,
    pub target: HitTarget,
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

    /// Remove menu rows as soon as a menu closes. The next frame restores
    /// the complete map; until then a fast second click must not replay a
    /// command through stale overlay geometry.
    pub fn clear_menu_items(&mut self) {
        self.regions
            .retain(|region| !matches!(region.target, HitTarget::MenuItem { .. }));
    }

    pub fn push_geometry(&mut self, geometry: PaneGeometry) {
        self.pane_geometries.push(geometry);
    }

    pub fn pane_geometry(&self, pane_id: &str) -> Option<&PaneGeometry> {
        self.pane_geometries.iter().find(|g| g.pane_id == pane_id)
    }

    pub fn push(&mut self, rect: Rect, target: HitTarget) {
        if rect.width > 0 && rect.height > 0 {
            self.regions.push(HitRegion { rect, target });
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
    /// [`HitMap::hit`] answers with the topmost region, and on a pane's top
    /// border that is the pane's own title strip or a split button. Those
    /// are real controls and keep their clicks, but the row they sit on is
    /// also the seam between two stacked panes, and a seam nobody can grab
    /// is a pane that can only be resized from its far edge. Measured: of
    /// the 40 cells on a two-pane seam's lower row, one was grabbable.
    ///
    /// Last pushed wins, the same rule `hit` uses, so the two can never
    /// disagree about which of two overlapping bands a cell belongs to.
    /// Bands from different levels of a split tree do not overlap today
    /// (an outer divider sits outside every child's rect), so the rule is
    /// only load-bearing if that ever stops being true.
    pub fn divider_at(&self, col: u16, row: u16) -> Option<(&str, SplitDir)> {
        self.regions
            .iter()
            .rev()
            .filter(|r| {
                col >= r.rect.x
                    && col < r.rect.x + r.rect.width
                    && row >= r.rect.y
                    && row < r.rect.y + r.rect.height
            })
            .find_map(|r| match &r.target {
                HitTarget::Divider { pane_id, dir } => Some((pane_id.as_str(), *dir)),
                _ => None,
            })
    }

    pub fn hit(&self, col: u16, row: u16) -> Option<&HitTarget> {
        self.regions
            .iter()
            .rev()
            .find(|r| {
                col >= r.rect.x
                    && col < r.rect.x + r.rect.width
                    && row >= r.rect.y
                    && row < r.rect.y + r.rect.height
            })
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
    WorkspaceMenu { session: String, at: (u16, u16) },
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

    #[test]
    fn hit_test_returns_topmost_region() {
        let mut map = HitMap::default();
        map.push(
            Rect::new(0, 0, 10, 10),
            HitTarget::Tab {
                window_id: "@0".into(),
            },
        );
        map.push(
            Rect::new(2, 2, 4, 4),
            HitTarget::PaneBody {
                pane_id: "%0".into(),
            },
        );
        assert!(matches!(map.hit(3, 3), Some(HitTarget::PaneBody { .. })));
        assert!(matches!(
            map.hit(9, 9),
            Some(HitTarget::Tab { window_id }) if window_id == "@0"
        ));
    }

    #[test]
    fn workspace_blocks_folds_agent_rows_into_their_own_workspace() {
        let mut map = HitMap::default();
        // $a is collapsed: one row, one block.
        map.push(
            Rect::new(0, 3, 20, 1),
            HitTarget::SidebarRow {
                session_id: "$a".into(),
                session: "a".into(),
            },
        );
        // $b is expanded: its header plus two agent rows fold into one
        // block spanning rows 4..7.
        map.push(
            Rect::new(0, 4, 20, 1),
            HitTarget::SidebarRow {
                session_id: "$b".into(),
                session: "b".into(),
            },
        );
        map.push(
            Rect::new(0, 5, 20, 1),
            HitTarget::SidebarAgent {
                workspace_id: "$b".into(),
                pane_id: "%1".into(),
                order_key: "pane:%1".into(),
            },
        );
        map.push(
            Rect::new(0, 6, 20, 1),
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
        map.push(
            Rect::new(0, 0, 10, 1),
            HitTarget::Tab {
                window_id: "@0".into(),
            },
        );
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
        map.push(Rect::new(0, 7, 1, 1), HitTarget::SidebarToggle);
        map.push(Rect::new(15, 7, 3, 1), HitTarget::NewWorkspaceButton);
        map.push(Rect::new(25, 0, 3, 1), HitTarget::NewTabButton);
        map.push(
            Rect::new(0, 0, 20, 5),
            HitTarget::PaneBody {
                pane_id: "%0".into(),
            },
        );

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
        map.push(
            Rect::new(0, 0, 10, 5),
            HitTarget::PaneBody {
                pane_id: "%0".into(),
            },
        );
        map.push(
            Rect::new(1, 1, 8, 1),
            HitTarget::MenuItem {
                action: BindingAction::ClosePane,
            },
        );

        map.clear_menu_items();

        assert!(matches!(map.hit(2, 1), Some(HitTarget::PaneBody { .. })));
    }
}
