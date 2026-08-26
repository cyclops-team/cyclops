//! Cyclops terminal workspace: pane VT runtimes and the full-screen TUI.

mod action;
pub mod animate;
pub mod app;
mod bindings;
mod config;
mod copy;
mod daemon;
mod decoration;
mod dialog;
mod drag;
pub mod event_record;
mod files;
mod input;
mod layout;
mod model;
mod naming;
mod notice;
mod persist;
mod render;
pub mod resilience;
pub mod runtime;
mod selection;
mod sound;
mod sync;
mod term_guard;
mod theme;

pub use app::{print_help_and_exit, run, run_async};
pub use render::{event_stream_rows, EventRow};
pub use runtime::{
    snapshot_from_bundle, CellAttrs, CellGrid, CellPos, Color, CursorShape, CursorState, GridCell,
    HydrationSnapshot, PaneRuntime, Underline,
};
