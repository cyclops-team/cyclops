//! Cyclops wire protocol and ledger schema.
//!
//! Data types only. No IO lives here. Both cyclopsd and every client compile
//! against this crate, so it is the single source of truth for what goes over
//! the socket and into the ledger.
//!
//! Compatibility rule (ADR-001, shpool pattern S2): the server writes a hello
//! line first on every connection. Version mismatch warns, never rejects.
//! All deserialization tolerates unknown fields.
//!
//! Rules over those types live here when every surface has to agree on
//! them. That includes the delivery state machine
//! ([`ledger::DeliveryState`]), the one-shot wake notification state
//! machine ([`notification::NotificationState`]), and what needs a human
//! ([`attention`], which owns the eye).
//!
//! ## What does not live here
//!
//! - Any behavior. Nothing here opens a file, a socket, or a process. A
//!   type that needed to would belong to whoever does the opening.
//! - The pipeline that drives the state machine (`cyclopsd/src/delivery.rs`)
//!   or the register that answers `status` (`cyclopsd/src/history.rs`).
//!   This crate says which moves are legal and which states need a human;
//!   it never makes a move.
//! - Anything about how a state LOOKS. Glyphs, words and color are
//!   `cyclops-theme` and `cyclops_ui::grid`.
//! - Detection rules. Those are `cyclops-manifest` data.
//!
//! One deliberate oddity: [`scratch`] is here and is used by nothing at
//! runtime. It holds the one statement of where throwaway test state goes,
//! and it sits in the crate every other crate already depends on.

pub mod attention;
pub mod build_identity;
pub mod identity;
pub mod label;
pub mod ledger;
pub mod mailbox;
pub mod notification;
pub mod scratch;
pub mod state;
pub mod wire;

pub use attention::*;
pub use build_identity::*;
pub use identity::*;
pub use ledger::*;
pub use mailbox::*;
pub use notification::*;
pub use state::*;
pub use wire::*;

/// Bumped only for breaking wire changes. Mismatch warns on both sides.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default socket path relative to the cyclops home directory.
pub const SOCK_NAME: &str = "sock";

/// Environment variable overriding the cyclops home (default `~/.cyclops`).
/// Tests point this at a scratch directory so nothing touches the real home.
pub const HOME_ENV: &str = "CYCLOPS_HOME";

/// Resolve the cyclops home directory: `$CYCLOPS_HOME` or `~/.cyclops`.
pub fn cyclops_home() -> std::path::PathBuf {
    if let Ok(p) = std::env::var(HOME_ENV) {
        if !p.is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join(".cyclops")
}

/// Socket path under the resolved home.
pub fn socket_path() -> std::path::PathBuf {
    cyclops_home().join(SOCK_NAME)
}

/// Nothing is answering the socket, and the command that fixes it.
///
/// Here, beside the socket path, because every surface that can fail to
/// reach the daemon has to say the same thing. The CLI and the stream
/// each carried their own copy of this sentence, and when `cyclops start`
/// took over starting the daemon, one of them would have kept telling
/// people to run `cyclopsd &`.
pub const NOT_RUNNING: &str = "cyclops isn't running. Start it with: cyclops start";
