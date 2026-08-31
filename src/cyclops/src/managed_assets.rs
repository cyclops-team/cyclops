//! Pure ownership decisions for the two setup-owned seeded assets.
//!
//! Detection manifests live under the Cyclops home and the agent skill lives
//! only below an installed consumer. They have one deliberately small rule in
//! common: Cyclops may create a missing file, but every existing file stays
//! in place. This module owns that byte-level decision. It neither reads nor
//! writes files, and does not turn hooks, themes, sounds, binaries, or
//! cleanup into a universal asset system.

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
    match seed_decision(Some(bytes), current, is_known_shipped).observed() {
        SeededAssetState::Shipped(state) => state,
        SeededAssetState::Missing | SeededAssetState::UnreadableOrUnproven => {
            unreachable!("readable seeded bytes have a shipped ownership state")
        }
    }
}

/// What a read-only inspection can establish about one seeded destination.
///
/// `Shipped` keeps the established setup-check spelling so planning and
/// checking describe the same observed ownership fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeededAssetState {
    Missing,
    Shipped(ShippedState),
    UnreadableOrUnproven,
}

impl SeededAssetState {
    /// Stable machine-readable spelling for planning.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Shipped(ShippedState::Current) => "current",
            Self::Shipped(ShippedState::KnownOld) => "outdated",
            Self::Shipped(ShippedState::OperatorEdited) => "operator_edited",
            Self::UnreadableOrUnproven => "unreadable_or_unproven",
        }
    }
}

/// The one safe setup action for a seeded destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeedAction {
    Create,
    KeepCurrent,
    PreserveKnownOldSeed,
    PreserveOperatorEdit,
    RefuseUnreadableOrUnproven,
}

impl SeedAction {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::KeepCurrent => "keep_current",
            Self::PreserveKnownOldSeed => "preserve_known_old_seed",
            Self::PreserveOperatorEdit => "preserve_operator_edit",
            Self::RefuseUnreadableOrUnproven => "manual_review_required",
        }
    }
}

/// A body-free decision produced from already-read bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeedDecision {
    observed: SeededAssetState,
    action: SeedAction,
}

impl SeedDecision {
    pub(crate) fn observed(self) -> SeededAssetState {
        self.observed
    }

    pub(crate) fn action(self) -> SeedAction {
        self.action
    }

    /// Why this action is safe. The caller owns the path; this owner only
    /// decides whether a missing shipped seed may be created.
    pub(crate) fn ownership_reason(self) -> &'static str {
        match self.action {
            SeedAction::Create => {
                "no file exists at this target, so Cyclops may create its shipped seed"
            }
            SeedAction::PreserveKnownOldSeed => {
                "the existing bytes match a released Cyclops seed, but automatic replacement is disabled; preserve them and review the update manually"
            }
            SeedAction::KeepCurrent => "the existing bytes match the current shipped seed",
            SeedAction::PreserveOperatorEdit => {
                "the existing bytes are not a released Cyclops seed, so the operator owns this edit"
            }
            SeedAction::RefuseUnreadableOrUnproven => {
                "Cyclops cannot safely establish ownership of this target, so it is left untouched and needs manual review"
            }
        }
    }
}

/// Decide from a missing or readable target. Callers pass their released
/// history predicate because manifest and skill histories must never be
/// conflated.
pub(crate) fn seed_decision(
    existing: Option<&[u8]>,
    current: &[u8],
    is_known_shipped: fn(&[u8]) -> bool,
) -> SeedDecision {
    let observed = match existing {
        None => SeededAssetState::Missing,
        Some(bytes) if bytes == current => SeededAssetState::Shipped(ShippedState::Current),
        Some(bytes) if is_known_shipped(bytes) => SeededAssetState::Shipped(ShippedState::KnownOld),
        Some(_) => SeededAssetState::Shipped(ShippedState::OperatorEdited),
    };
    decision_for(observed)
}

/// An inspection could not safely obtain bytes. The only honest plan is to
/// leave the target untouched; a later setup run will still surface its own
/// exact IO error without this preview ever mutating state.
pub(crate) fn refuse_unreadable_or_unproven() -> SeedDecision {
    decision_for(SeededAssetState::UnreadableOrUnproven)
}

fn decision_for(observed: SeededAssetState) -> SeedDecision {
    let action = match observed {
        SeededAssetState::Missing => SeedAction::Create,
        SeededAssetState::Shipped(ShippedState::Current) => SeedAction::KeepCurrent,
        SeededAssetState::Shipped(ShippedState::KnownOld) => SeedAction::PreserveKnownOldSeed,
        SeededAssetState::Shipped(ShippedState::OperatorEdited) => SeedAction::PreserveOperatorEdit,
        SeededAssetState::UnreadableOrUnproven => SeedAction::RefuseUnreadableOrUnproven,
    };
    SeedDecision { observed, action }
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
    fn plans_each_safe_seed_action_from_body_free_ownership_facts() {
        let current = b"current";
        assert_eq!(
            seed_decision(None, current, current_or_old).action(),
            SeedAction::Create
        );
        assert_eq!(
            seed_decision(Some(b"old"), current, current_or_old).action(),
            SeedAction::PreserveKnownOldSeed
        );
        assert_eq!(
            seed_decision(Some(current), current, current_or_old).action(),
            SeedAction::KeepCurrent
        );
        assert_eq!(
            seed_decision(Some(b"operator edit"), current, current_or_old).action(),
            SeedAction::PreserveOperatorEdit
        );
        assert_eq!(
            refuse_unreadable_or_unproven().action(),
            SeedAction::RefuseUnreadableOrUnproven
        );
    }

    #[test]
    fn only_current_and_operator_edits_are_ready() {
        assert!(ShippedState::Current.ready());
        assert!(!ShippedState::KnownOld.ready());
        assert!(ShippedState::OperatorEdited.ready());
        assert_eq!(ShippedState::KnownOld.word(), "outdated");
    }

    /// A syntactic boundary check only: the two writers and the two existing
    /// inspectors name this owner rather than carrying their own ownership
    /// rule. The tests above and the CLI suites cover the resulting behavior.
    #[test]
    fn seeded_asset_callers_name_the_shared_ownership_owner() {
        let source_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for caller in ["setup.rs", "health.rs"] {
            let source = std::fs::read_to_string(source_dir.join(caller))
                .expect("read classifier caller source");
            assert!(
                source.contains("crate::managed_assets::classify_seeded_bytes("),
                "{caller} must delegate seeded-byte classification to managed_assets"
            );
        }
        for caller in ["manifests.rs", "skillseed.rs"] {
            let source =
                std::fs::read_to_string(source_dir.join(caller)).expect("read seed caller source");
            assert!(
                source.contains("crate::managed_assets::seed_decision("),
                "{caller} must delegate seeded write decisions to managed_assets"
            );
        }
    }
}
