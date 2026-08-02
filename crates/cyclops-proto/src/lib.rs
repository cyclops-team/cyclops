//! Cyclops wire protocol and ledger schema.
//!
//! Data types only. No IO lives here. Both cyclopsd and every client compile
//! against this crate, so it is the single source of truth for what goes over
//! the socket and into the ledger.
//!
//! Compatibility rule (ADR-001, shpool pattern S2): the server writes a hello
//! line first on every connection. Version mismatch warns, never rejects.
//! All deserialization tolerates unknown fields.

pub mod ledger;
pub mod state;
pub mod wire;

pub use ledger::*;
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
