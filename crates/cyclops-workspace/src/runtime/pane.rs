//! One live pane's VT runtime and scroll/selection state.

use super::alacritty::AlacrittyVt;
use super::grid::{CellGridView, CellPos, CursorState, HydrationSnapshot};

/// Wraps the pane VT engine with hydration and viewport state.
pub struct PaneRuntime {
    vt: AlacrittyVt,
    scroll_offset: usize,
}

impl PaneRuntime {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            vt: AlacrittyVt::new(cols, rows),
            scroll_offset: 0,
        }
    }

    /// Hydrate from a tmux bundle. Visual snapshot only — not parser-exact.
    pub fn hydrate(&mut self, snapshot: &HydrationSnapshot) {
        self.vt.hydrate(snapshot);
        self.scroll_offset = 0;
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.vt.feed(bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.vt.resize(cols, rows);
    }

    pub fn grid(&self) -> CellGridView<'_> {
        self.vt.grid()
    }

    pub fn cursor(&self) -> CursorState {
        self.vt.cursor()
    }

    pub fn scroll(&mut self, delta: i32) {
        self.vt.scroll(delta);
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(delta.unsigned_abs() as usize);
    }

    pub fn select(&mut self, from: CellPos, to: CellPos) -> Option<String> {
        self.vt.select(from, to)
    }

    /// Rehydrate after flow-control pause: never trust resumed byte continuity.
    pub fn rehydrate(&mut self, snapshot: &HydrationSnapshot) {
        self.hydrate(snapshot);
    }
}

/// Map a tmux adapter bundle into workspace hydration state.
pub fn snapshot_from_bundle(bundle: &cyclops_tmux::HydrationBundle) -> HydrationSnapshot {
    HydrationSnapshot {
        cols: bundle.cols,
        rows: bundle.rows,
        visible: bundle.visible_escaped.clone(),
        alternate: bundle.alternate_escaped.clone(),
        cursor_x: bundle.cursor_x,
        cursor_y: bundle.cursor_y,
        alternate_on: bundle.alternate_on,
    }
}
