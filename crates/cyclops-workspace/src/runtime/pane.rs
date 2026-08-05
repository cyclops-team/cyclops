//! One live pane's VT runtime and scroll/selection state.

use super::alacritty::AlacrittyVt;
use super::grid::{CellGrid, CellGridView, CellPos, CursorState, HydrationSnapshot};

/// Wraps the pane VT engine with hydration and viewport state.
pub struct PaneRuntime {
    vt: AlacrittyVt,
}

impl PaneRuntime {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            vt: AlacrittyVt::new(cols, rows),
        }
    }

    /// Hydrate from a tmux bundle. Visual snapshot only — not parser-exact.
    pub fn hydrate(&mut self, snapshot: &HydrationSnapshot) {
        self.vt.hydrate(snapshot);
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.vt.feed(bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.vt.resize(cols, rows);
    }

    /// Grid dimensions as `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        self.vt.size()
    }

    /// Whether the viewport is at the live tail (not scrolled into history).
    pub fn at_tail(&self) -> bool {
        self.vt.at_tail()
    }

    pub fn grid(&mut self) -> CellGridView<'_> {
        self.vt.grid()
    }

    /// An owned copy of the visible grid, for tests that need owned values.
    pub fn snapshot(&mut self) -> CellGrid {
        self.vt.snapshot()
    }

    pub fn cursor(&self) -> CursorState {
        self.vt.cursor()
    }

    pub fn scroll(&mut self, delta: i32) {
        self.vt.scroll(delta);
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
///
/// `capture-pane -a` reads tmux's *saved* grid, which is the primary screen
/// from before a TUI entered the alternate buffer — not the alternate buffer
/// itself. The adapter still spells that field `alternate_escaped`; the
/// workspace names it for what it holds (F38).
pub fn snapshot_from_bundle(bundle: &cyclops_tmux::HydrationBundle) -> HydrationSnapshot {
    HydrationSnapshot {
        cols: bundle.cols,
        rows: bundle.rows,
        visible: bundle.visible_escaped.clone(),
        saved_primary: bundle.alternate_escaped.clone(),
        cursor_x: bundle.cursor_x,
        cursor_y: bundle.cursor_y,
        alternate_on: bundle.alternate_on,
    }
}
