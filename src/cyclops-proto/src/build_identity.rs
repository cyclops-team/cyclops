//! Compile-time source identity shared by every Cyclops component.
//!
//! `Cargo.toml` remains the sole authority for the workspace version. This
//! module only exposes the one source stamp produced by this crate's build
//! script and the combined string needed by binary `--version` output.

/// Short source identity stamped once for the whole workspace.
pub const BUILD_REF: &str = env!("CYCLOPS_BUILD_REF");

/// Full source identity used by retained release evidence.
pub const BUILD_ID: &str = env!("CYCLOPS_BUILD_ID");

/// Cargo workspace version followed by the shared source identity.
pub const VERSION_WITH_BUILD: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CYCLOPS_BUILD_REF"),
    ")"
);
