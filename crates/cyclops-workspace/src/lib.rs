//! Cyclops terminal workspace: pane VT runtimes and the full-screen TUI.
//!
//! Step 1 owns the pane VT engine and fixture corpus. Later steps add the
//! Ratatui chrome, tmux control-mode wiring, and CLI dispatch.

pub mod runtime;

pub use runtime::{
    feed_alacritty, AlacrittyVt, CellAttrs, CellGrid, CellGridView, CellPos, Color, CursorShape,
    CursorState, GridCell, HydrationSnapshot,
};
