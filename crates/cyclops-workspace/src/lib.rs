//! Cyclops terminal workspace: pane VT runtimes and the full-screen TUI.

mod action;
pub mod app;
mod bindings;
mod config;
mod copy;
mod daemon;
mod decoration;
mod dialog;
mod drag;
mod input;
mod intent;
mod layout;
mod model;
mod persist;
mod render;
pub mod resilience;
pub mod runtime;
mod selection;
mod sync;
mod term_guard;
mod theme;

pub use app::{print_help_and_exit, run, run_async};
pub use runtime::{
    feed_alacritty, snapshot_from_bundle, AlacrittyVt, CellAttrs, CellGrid, CellGridView, CellPos,
    Color, CursorShape, CursorState, GridCell, HydrationSnapshot, PaneRuntime, Underline,
};
