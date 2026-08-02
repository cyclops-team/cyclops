//! The tmux adapter (ADR-001 consequence: tmux specifics confined to one
//! module, version-gated, CI'd against tmux HEAD).
//!
//! Implemented in M0: version probe, control-mode client, reconciling pane
//! state table. Placeholder until the M0 implementation lands.

pub mod version;

pub use version::TmuxVersion;
