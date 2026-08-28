//! Drag state machine for dividers, panes, tabs, and sidebar row ordering.

use crate::action::Insertion;
use crate::layout::SplitDir;

/// What a drag operation targets once it crosses the movement threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragTarget {
    Divider {
        pane_id: String,
        dir: SplitDir,
        /// The pane a below-threshold release focuses.
        ///
        /// `None` for the bare gutter, which has no pane to focus. `Some`
        /// when the seam was grabbed through a pane's own top border: the
        /// title strip painted there is a focus control and keeps its
        /// click, while the row it sits on stays the resize handle it also
        /// is. The pane focused is the one whose border was pressed, never
        /// `pane_id` — that is the pane on the far side of the seam, the
        /// one `resize-pane` targets.
        focus_on_click: Option<String>,
    },
    /// Swap a pane with the one it is dropped on. Picked up by its corner
    /// grip, never its frame or body: a frame click focuses and a body
    /// drag is text selection.
    Pane {
        pane_id: String,
    },
    Tab {
        window_id: String,
    },
    Workspace {
        session_id: String,
        session: String,
    },
    Agent {
        workspace_id: String,
        pane_id: String,
        order_key: String,
    },
    /// Resize the application sidebar, not a tmux pane.
    Sidebar,
    /// Resize the right-edge Messages pane.
    Messages,
    /// Move the seam between the sidebar's session tree and its file
    /// panel. Vertical only, and it resizes no tmux pane: both halves
    /// belong to the application.
    SidebarSplit,
    /// Move the open dialog off whatever it is covering. Picked up by the
    /// box's top border and title row; the rest of the card belongs to the
    /// controls painted on it.
    Dialog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragPhase {
    Down,
    Moving,
}

/// One in-flight drag: down → threshold → move → up, or Escape cancel.
#[derive(Debug, Clone)]
pub struct DragState {
    pub target: DragTarget,
    pub phase: DragPhase,
    pub start: (u16, u16),
    pub current: (u16, u16),
    /// Position already translated into tmux operations — divider drags
    /// apply live, coalescing motion since the last applied step.
    pub last_applied: (u16, u16),
}

pub const DRAG_THRESHOLD_PX: u16 = 3;

impl DragState {
    pub fn on_down(target: DragTarget, x: u16, y: u16) -> Self {
        DragState {
            target,
            phase: DragPhase::Down,
            start: (x, y),
            current: (x, y),
            last_applied: (x, y),
        }
    }

    pub fn on_move(&mut self, x: u16, y: u16) {
        self.current = (x, y);
        if self.phase == DragPhase::Down && self.past_threshold() {
            self.phase = DragPhase::Moving;
        }
    }

    pub fn past_threshold(&self) -> bool {
        let (sx, sy) = self.start;
        let (cx, cy) = self.current;
        let threshold = match &self.target {
            // Sidebar rows are one cell apart; requiring the general
            // three-cell threshold would make adjacent rows impossible to
            // reorder with a natural vertical drag.
            DragTarget::Workspace { .. }
            | DragTarget::Agent { .. }
            | DragTarget::Sidebar
            | DragTarget::Messages
            | DragTarget::SidebarSplit => 1,
            _ => DRAG_THRESHOLD_PX,
        };
        sx.abs_diff(cx) >= threshold || sy.abs_diff(cy) >= threshold
    }

    /// Commit on mouse up. None when the drag never crossed the threshold.
    pub fn on_up(&self) -> Option<DragTarget> {
        if !self.past_threshold() {
            return None;
        }
        Some(self.target.clone())
    }

    pub fn is_active(&self) -> bool {
        self.phase == DragPhase::Moving
    }
}

/// One workspace row's vertical span in the sidebar, as the last frame
/// actually painted it — see
/// [`crate::input::mouse::HitMap::workspace_blocks`], the only place that
/// builds these. An expanded workspace's agent rows fold into their own
/// header's block rather than getting one each, so the boundary math below
/// only ever sits between two WORKSPACES, never inside one's agent rows. A
/// workspace the sidebar clipped for lack of height contributes no block,
/// so a drag can only preview a slot the sidebar actually shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBlock {
    pub session_id: String,
    pub top: u16,
    /// Exclusive: the first row after this block, whether that is the next
    /// workspace's own row or the sidebar's empty footer space.
    pub bottom: u16,
}

/// Which insertion boundary `pointer_row` currently previews: an index into
/// `0..=blocks.len()`. `0` is before the first block, `blocks.len()` is
/// after the last, and any other value `i` sits between `blocks[i - 1]`
/// and `blocks[i]`.
///
/// The split point inside each block is its own vertical midpoint, so an
/// expanded workspace's whole row-plus-agents span counts as one unit: the
/// pointer has to cross past half of THAT WHOLE BLOCK before the boundary
/// jumps to its far side, and it can never land between the header row and
/// one of its own agent rows. Callers are expected to have already checked
/// that `pointer_row` sits inside the sidebar; this function does not know
/// the sidebar's bounds, so an out-of-range row still clamps to slot `0` or
/// `blocks.len()` rather than panicking.
pub fn slot_for_row(blocks: &[WorkspaceBlock], pointer_row: u16) -> usize {
    for (i, block) in blocks.iter().enumerate() {
        let mid = block.top + (block.bottom - block.top) / 2;
        if pointer_row <= mid {
            return i;
        }
    }
    blocks.len()
}

/// The row the insertion rule paints on for `slot`: the seam between
/// `blocks[slot - 1]` and `blocks[slot]`, or the sidebar's very first/last
/// occupied row at either end. `None` when `blocks` is empty — nothing to
/// draw a boundary between.
pub fn boundary_row(blocks: &[WorkspaceBlock], slot: usize) -> Option<u16> {
    if slot == 0 {
        blocks.first().map(|b| b.top)
    } else if slot >= blocks.len() {
        blocks.last().map(|b| b.bottom)
    } else {
        Some(blocks[slot].top)
    }
}

/// The stable-id [`Insertion`] `slot` represents for the block whose
/// session id is `source`. `None` covers two cases a caller should treat
/// identically — clear the preview/skip the drop, never fall back to a
/// different resolution: `source` is not among `blocks` (a stale drag
/// against a row that has since disappeared), or `slot` is one of the two
/// boundaries touching `source`'s own block, which drops it exactly where
/// it already sits — not a move, so there is nothing to dispatch.
pub fn insertion_for_slot(
    blocks: &[WorkspaceBlock],
    source: &str,
    slot: usize,
) -> Option<Insertion> {
    let source_index = blocks.iter().position(|b| b.session_id == source)?;
    if slot == source_index || slot == source_index + 1 {
        return None;
    }
    if slot < source_index {
        Some(Insertion::Before(blocks[slot].session_id.clone()))
    } else {
        Some(Insertion::After(blocks[slot - 1].session_id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn divider() -> DragTarget {
        DragTarget::Divider {
            pane_id: "%0".into(),
            dir: SplitDir::Horizontal,
            focus_on_click: None,
        }
    }

    #[test]
    fn down_to_up_without_threshold_commits_nothing() {
        let mut drag = DragState::on_down(divider(), 10, 10);
        drag.on_move(11, 10);
        assert!(!drag.past_threshold());
        assert!(drag.on_up().is_none());
    }

    #[test]
    fn threshold_then_move_then_up() {
        let mut drag = DragState::on_down(divider(), 0, 0);
        drag.on_move(5, 0);
        assert_eq!(drag.phase, DragPhase::Moving);
        let committed = drag.on_up().expect("commit");
        assert!(matches!(committed, DragTarget::Divider { .. }));
    }

    #[test]
    fn adjacent_sidebar_rows_cross_the_reorder_threshold() {
        let mut drag = DragState::on_down(
            DragTarget::Workspace {
                session_id: "$0".into(),
                session: "main".into(),
            },
            4,
            4,
        );
        drag.on_move(4, 5);
        assert!(drag.is_active());
    }

    #[test]
    fn a_one_cell_messages_drag_resizes_instead_of_feeling_stuck() {
        let mut drag = DragState::on_down(DragTarget::Messages, 80, 0);
        drag.on_move(79, 0);
        assert!(drag.is_active());
        assert_eq!(drag.on_up(), Some(DragTarget::Messages));
    }

    #[test]
    fn each_target_variant_lifecycle() {
        for target in [
            DragTarget::Pane {
                pane_id: "%0".into(),
            },
            DragTarget::Tab {
                window_id: "@0".into(),
            },
            DragTarget::Workspace {
                session_id: "$0".into(),
                session: "main".into(),
            },
            DragTarget::Agent {
                workspace_id: "$0".into(),
                pane_id: "%0".into(),
                order_key: "name:reviewer".into(),
            },
            divider(),
            DragTarget::Sidebar,
        ] {
            let mut drag = DragState::on_down(target.clone(), 0, 0);
            drag.on_move(10, 10);
            assert!(drag.is_active());
            assert_eq!(drag.on_up(), Some(target));
        }
    }

    // -- Workspace-row slot math: first, middle, and final insertion
    // slots; expanded blocks count as one unit. --

    fn block(session_id: &str, top: u16, bottom: u16) -> WorkspaceBlock {
        WorkspaceBlock {
            session_id: session_id.to_string(),
            top,
            bottom,
        }
    }

    /// Three collapsed (height-1) workspace rows, one after another.
    fn three_collapsed() -> Vec<WorkspaceBlock> {
        vec![block("$a", 2, 3), block("$b", 3, 4), block("$c", 4, 5)]
    }

    #[test]
    fn slot_for_row_before_the_first_block_is_slot_zero() {
        let blocks = three_collapsed();
        assert_eq!(slot_for_row(&blocks, 0), 0);
        assert_eq!(slot_for_row(&blocks, 2), 0, "on the first row itself");
    }

    #[test]
    fn slot_for_row_between_two_collapsed_rows_is_the_middle_slot() {
        let blocks = three_collapsed();
        // Row 3 is $b's own row; landing on it previews inserting before
        // it, i.e. right after $a.
        assert_eq!(slot_for_row(&blocks, 3), 1);
    }

    #[test]
    fn slot_for_row_past_the_last_block_is_the_final_slot() {
        let blocks = three_collapsed();
        assert_eq!(slot_for_row(&blocks, 4), 2, "on $c's own row");
        assert_eq!(slot_for_row(&blocks, 5), 3, "below every row");
        assert_eq!(slot_for_row(&blocks, 50), 3, "far below every row");
    }

    #[test]
    fn expanded_block_splits_on_its_own_midpoint_not_a_sub_row() {
        // $a is expanded with two agent rows: header (2) + agents (3, 4)
        // fold into one block spanning [2, 5). $b starts right after.
        let blocks = vec![block("$a", 2, 5), block("$b", 5, 6)];

        // Header row and the first agent row are the "top half" — both
        // preview inserting before the whole $a block.
        assert_eq!(slot_for_row(&blocks, 2), 0, "on $a's header row");
        assert_eq!(slot_for_row(&blocks, 3), 0, "on $a's first agent row");
        // The second (last) agent row is the "bottom half" — previews
        // after the whole block, never a boundary inside it.
        assert_eq!(slot_for_row(&blocks, 4), 1, "on $a's second agent row");
    }

    #[test]
    fn a_workspace_the_sidebar_clipped_contributes_no_slot() {
        // Only two of three workspaces fit on screen — the third has no
        // block at all, matching how `paint_sidebar` clips rather than
        // scrolls. Even hovering far past the visible rows can only reach
        // "after the last VISIBLE block", never a slot implying the
        // clipped row.
        let blocks = vec![block("$a", 2, 3), block("$b", 3, 4)];
        assert_eq!(
            slot_for_row(&blocks, 4),
            2,
            "just past the last visible row"
        );
        assert_eq!(slot_for_row(&blocks, 100), 2, "clamps at the visible end");
    }

    // -- Insertion resolution from a slot: matches the previewed rule, and
    // is a no-op exactly when the drop does not move anything. --

    #[test]
    fn insertion_for_slot_before_the_first_row() {
        let blocks = three_collapsed();
        assert_eq!(
            insertion_for_slot(&blocks, "$c", 0),
            Some(Insertion::Before("$a".into()))
        );
    }

    #[test]
    fn insertion_for_slot_after_the_last_row() {
        let blocks = three_collapsed();
        assert_eq!(
            insertion_for_slot(&blocks, "$a", 3),
            Some(Insertion::After("$c".into()))
        );
    }

    #[test]
    fn insertion_for_slot_between_two_middle_rows() {
        let blocks = vec![
            block("$a", 0, 1),
            block("$b", 1, 2),
            block("$c", 2, 3),
            block("$d", 3, 4),
        ];
        // Dragging $d up to slot 1 (between $a and $b) inserts before $b.
        assert_eq!(
            insertion_for_slot(&blocks, "$d", 1),
            Some(Insertion::Before("$b".into()))
        );
        // Dragging $a down to slot 3 (between $c and $d) inserts after $c.
        assert_eq!(
            insertion_for_slot(&blocks, "$a", 3),
            Some(Insertion::After("$c".into()))
        );
    }

    #[test]
    fn insertion_for_slot_touching_its_own_position_is_a_no_op() {
        let blocks = three_collapsed();
        // $b sits at index 1; both boundaries that touch it (slot 1 itself,
        // and slot 2 right after it) leave it exactly where it is.
        assert_eq!(insertion_for_slot(&blocks, "$b", 1), None);
        assert_eq!(insertion_for_slot(&blocks, "$b", 2), None);
    }

    #[test]
    fn insertion_for_slot_against_a_vanished_source_resolves_nothing() {
        let blocks = three_collapsed();
        assert_eq!(insertion_for_slot(&blocks, "$missing", 0), None);
    }

    #[test]
    fn boundary_row_matches_the_block_edges_it_sits_between() {
        let blocks = three_collapsed();
        assert_eq!(boundary_row(&blocks, 0), Some(2), "before the first row");
        assert_eq!(boundary_row(&blocks, 1), Some(3), "between $a and $b");
        assert_eq!(boundary_row(&blocks, 3), Some(5), "after the last row");
        assert_eq!(boundary_row(&[], 0), None, "nothing to bound with no rows");
    }

    #[tokio::test]
    async fn divider_drag_resize_converges_on_rig() {
        use cyclops_testrig::{tmux_available, TmuxServer};
        use cyclops_tmux::{ControlClient, PaneDirection};

        if !tmux_available() {
            return;
        }
        let server = TmuxServer::new("drag-divider");
        server.run_ok(&["new-session", "-d", "-s", "d", "/bin/sh"]);
        server.run_ok(&["split-window", "-h", "-t", "d"]);
        let cfg = cyclops_tmux::ControlConfig::attach("d")
            .on_socket(server.socket().to_string())
            .with_config_file("/dev/null");
        let (client, _rx) = ControlClient::spawn(cfg).await.expect("attach");
        let out = server.run(&["list-panes", "-t", "d", "-F", "#{pane_id}"]);
        let pane = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("%0")
            .trim()
            .to_string();
        // The divider's resolved direction for a positive horizontal delta —
        // mirrors what `action::resolve_pane_resize` would hand the executor.
        client
            .resize_pane(&pane, PaneDirection::Right, 3)
            .await
            .expect("resize");
        let sizes: Vec<String> = String::from_utf8_lossy(
            &server
                .run(&["list-panes", "-t", "d", "-F", "#{pane_width}"])
                .stdout,
        )
        .lines()
        .map(str::to_string)
        .collect();
        assert_eq!(sizes.len(), 2);
        assert_ne!(
            sizes[0], sizes[1],
            "divider resize should change pane widths"
        );
        client.shutdown().await;
    }
}
