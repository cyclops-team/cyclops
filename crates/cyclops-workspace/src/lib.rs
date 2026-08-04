//! Cyclops terminal workspace: pane VT runtimes and the full-screen TUI.

pub mod app;
mod bindings;
mod config;
mod daemon;
mod dialog;
mod copy;
mod input;
mod intent;
mod layout;
mod model;
mod render;
pub mod resilience;
pub mod runtime;
mod sync;
mod term_guard;
mod theme;

pub use app::{print_help_and_exit, run, run_async};
pub use runtime::{
    feed_alacritty, snapshot_from_bundle, AlacrittyVt, CellAttrs, CellGrid, CellGridView, CellPos,
    Color, CursorShape, CursorState, GridCell, HydrationSnapshot, PaneRuntime,
};
