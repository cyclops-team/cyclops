//! The tmux adapter, and the blast wall around it.
//!
//! ## The rule
//!
//! **Nothing outside this crate speaks to tmux.** Every `tmux` process this
//! product starts, every control-mode connection it holds, every command
//! line it writes and every reply it parses is in here. A caller says what
//! it wants in this crate's vocabulary; how that becomes tmux is not its
//! business.
//!
//! ADR-001 is why: control mode is unversioned and moves between releases,
//! so a change tmux makes has to be absorbable in one crate. When it moves,
//! the diff is here or the design failed.
//!
//! Two things the rule does NOT say, because both would be false:
//!
//! - It does not say callers hold no tmux words.
//!   `cyclopsd/src/chrome.rs` names the two border options it writes and
//!   composes their values, then sends them through [`ControlClient`]. It
//!   is allowed to know that vocabulary; it is not allowed to reach tmux
//!   without this crate.
//! - It is not perfectly kept today. `cyclopsd::probe_tmux` runs `tmux -V`
//!   itself and hands the text to [`TmuxVersion::parse`], so the one
//!   invocation this crate does not own is the version probe, and it goes
//!   out without the `-u` that `cmd::run` puts on every other one. It
//!   is the exception, it is named here so the boundary stays findable,
//!   and it is the thing to fix rather than to copy.
//!
//! ## What lives here, bottom up
//!
//! - [`quote_arg`]: the one quoting rule for tmux command arguments.
//! - [`Notification`]: parsed control-mode notifications; unknown lines are
//!   data, never errors.
//! - [`ControlClient`]: one `tmux -C` child, FIFO reply correlation, flow
//!   control via pause-after, typed helpers (capture, display, send-keys,
//!   load-buffer, paste-buffer). The stream is read as raw byte lines:
//!   %output data is not guaranteed to be valid UTF-8 on the wire (F22).
//! - [`SessionWatcher`]: zero-polling reconciling pane table for one
//!   session. `refresh-client -B` subscriptions (MEASURED working on tmux
//!   3.6a) push per-pane field changes; structural notifications are hints
//!   that trigger debounced reconciliation against `list-panes`.
//! - [`TmuxVersion`]: version parsing and feature gates.
//! - [`focus_pane`]: one-shot focus jump for the stream UI, outside
//!   control mode on purpose (a user gesture, not daemon state).
//! - [`layout`]: workspace layouts, read off a session and applied to a
//!   new one. Not re-exported flat: its `Server` is one of several types
//!   in this workspace that name a tmux server, and the module path is
//!   what tells them apart.
//!
//! ## What does NOT live here
//!
//! - Any judgement about what a pane is doing. This crate reports fields;
//!   `cyclopsd/src/fusion.rs` decides what they mean, and the rules it
//!   decides with are `cyclops-manifest` data.
//! - Anything about deliveries, the ledger, labels, or themes. A pane id
//!   is the widest thing this crate knows about a message.
//! - Test tmux servers. The isolated server and its teardown rule are
//!   `cyclops-testrig`'s, including for this crate's own tests.
//!
//! Everything can fail with [`TmuxError`].

pub mod control;
pub mod error;
pub mod focus;
pub mod hydration;
pub mod layout;
pub mod notify;
pub mod quote;
pub mod session;
pub mod version;
pub mod watcher;

mod cmd;

pub use control::{ControlClient, ControlConfig, ControlMode};
pub use error::TmuxError;
pub use focus::focus_pane;
pub use hydration::HydrationBundle;
pub use notify::Notification;
pub use quote::quote_arg;
pub use session::{active_pane, list_sessions, SessionRow};
pub use version::TmuxVersion;
pub use watcher::{PaneEvent, PaneField, PaneRow, SessionWatcher};
