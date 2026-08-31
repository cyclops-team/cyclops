//! Read-only drift facts shared by setup inspectors.
//!
//! `setup check` and `health` use different filesystem boundaries: setup
//! reports ordinary local setup state, while health refuses unproven state
//! paths. Both need the same answer after bytes have been read safely: are
//! they the current shipped copy, a known older shipped copy, or an operator
//! edit? This module owns only that answer. It neither reads nor writes files,
//! and does not try to make binaries, hook configurations, or other assets
//! share one lifecycle state machine.

/// Ownership and drift for bytes seeded by a Cyclops release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShippedState {
    Current,
    KnownOld,
    OperatorEdited,
}

impl ShippedState {
    /// Existing machine-readable spelling. `outdated` means a known old
    /// Cyclops seed, not an arbitrary file that happens to differ.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::KnownOld => "outdated",
            Self::OperatorEdited => "edited",
        }
    }

    /// An operator's preserved edit remains usable; a known old seed needs
    /// an explicit refresh before it can claim current setup.
    pub(crate) fn ready(self) -> bool {
        matches!(self, Self::Current | Self::OperatorEdited)
    }
}

/// Classify already-read bytes against one current body and the asset's
/// released-history predicate.
///
/// Current bytes must be checked first because each asset's history includes
/// the current release as an unedited seed too.
pub(crate) fn classify_seeded_bytes(
    bytes: &[u8],
    current: &[u8],
    is_known_shipped: fn(&[u8]) -> bool,
) -> ShippedState {
    if bytes == current {
        ShippedState::Current
    } else if is_known_shipped(bytes) {
        ShippedState::KnownOld
    } else {
        ShippedState::OperatorEdited
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn current_or_old(bytes: &[u8]) -> bool {
        matches!(bytes, b"current" | b"old")
    }

    #[test]
    fn classifies_current_known_old_and_operator_edits_without_losing_current() {
        let current = b"current";
        assert_eq!(
            classify_seeded_bytes(current, current, current_or_old),
            ShippedState::Current,
            "the released-history predicate includes the current seed"
        );
        assert_eq!(
            classify_seeded_bytes(b"old", current, current_or_old),
            ShippedState::KnownOld
        );
        assert_eq!(
            classify_seeded_bytes(b"operator edit", current, current_or_old),
            ShippedState::OperatorEdited
        );
    }

    #[test]
    fn only_current_and_operator_edits_are_ready() {
        assert!(ShippedState::Current.ready());
        assert!(!ShippedState::KnownOld.ready());
        assert!(ShippedState::OperatorEdited.ready());
        assert_eq!(ShippedState::KnownOld.word(), "outdated");
    }

    /// A syntactic boundary check only: both user-facing inspectors must use
    /// this owner for seeded-byte classification. The tests above and the CLI
    /// suites cover the resulting behavior.
    #[test]
    fn setup_and_health_name_the_shared_seeded_byte_classifier() {
        let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for caller in ["setup.rs", "health.rs"] {
            let source = std::fs::read_to_string(source_dir.join(caller))
                .expect("read classifier caller source");
            assert!(
                source.contains("crate::managed_assets::classify_seeded_bytes("),
                "{caller} must delegate seeded-byte classification to managed_assets"
            );
        }
    }
}
