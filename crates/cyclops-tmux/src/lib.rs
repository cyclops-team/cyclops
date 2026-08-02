//! The tmux adapter: every tmux-specific behavior in Cyclops lives in this
//! crate and nowhere else (ADR-001 consequence: control mode is unversioned
//! and under active refactor, so this crate is the blast wall).
//!
//! Layers, bottom up:
//!
//! - [`quote_arg`]: the one quoting rule for tmux command arguments.
//! - [`Notification`]: parsed control-mode notifications; unknown lines are
//!   data, never errors.
//! - [`ControlClient`]: one `tmux -C` child, FIFO reply correlation, flow
//!   control via pause-after, typed helpers (capture, display, send-keys,
//!   load-buffer, paste-buffer).
//! - [`SessionWatcher`]: zero-polling reconciling pane table for one
//!   session. `refresh-client -B` subscriptions (MEASURED working on tmux
//!   3.6a) push per-pane field changes; structural notifications are hints
//!   that trigger debounced reconciliation against `list-panes`.
//! - [`TmuxVersion`]: version probe and feature gates.
//!
//! Everything can fail with [`TmuxError`].

pub mod control;
pub mod error;
pub mod notify;
pub mod quote;
pub mod version;
pub mod watcher;

pub use control::{ControlClient, ControlConfig, ControlMode};
pub use error::TmuxError;
pub use notify::Notification;
pub use quote::quote_arg;
pub use version::TmuxVersion;
pub use watcher::{PaneEvent, PaneField, PaneRow, SessionWatcher};
