//! The agent skill that ships with the binary, and getting it into the
//! agent's own skill folder.
//!
//! The skill (`skills/cyclops/SKILL.md` in the repo) is the one file that
//! teaches a coding agent the cyclops verbs and the safety rules. Without
//! it an agent asked to "use the cyclops skill" has nothing to use: the
//! file lived only in the repo, so every install shipped a messaging
//! system its own agents had never heard of. It is compiled in here the
//! same way `manifests.rs` compiles in the detection manifests, and for
//! the same reason: an install is two binaries, and it has to work before
//! it has files.
//!
//! Where it goes is another tool's skill directory,
//! which `crate::hookset` deliberately refuses to touch for `hooks
//! install`. Writing here follows the rules `hookset::wire_vendor`
//! already set for vendor homes: only under the installer's
//! `--wire-hooks` consent (given at install time, or recorded then and
//! honored by a later boot: `workspace::finish_deferred_wiring`), only when
//! a consumer's own directory already exists, honoring
//! `CYCLOPS_NO_VENDOR_HOOKS`, and creating only a missing shipped skill.
//! Every existing skill, including a known old Cyclops seed, remains
//! untouched.

use std::path::{Path, PathBuf};

use cyclops_state::{CreateFileOutcome, ManagedAssetRoot, StateError, StateInspector};

/// The skill body the binary carries.
pub(crate) const SHIPPED: &str = include_str!("../../../skills/cyclops/SKILL.md");

/// FNV-1a 64 of every skill body this project has released, the current
/// one included. Same classification contract as
/// `crate::manifests::EVER_SHIPPED_FNV64`: a file on disk whose hash is in
/// this list is a known old Cyclops seed; any other existing content is an
/// operator edit. Both remain untouched. The test below fails until the
/// current body is listed, and prints the hash to append.
///
/// Released, not every body that ever compiled. An unreleased intermediate
/// was never seeded onto an operator machine, so listing it would only
/// misclassify bytes that could just as well be an operator's own.
const EVER_SHIPPED_FNV64: &[&str] = &[
    "7ebc1453af11b931",
    "cf5916d45a60081c",
    "2a2b7ed80b7a8f81",
    "a765a5ba2ec5ede0",
    "74bc4c099fc6dd15",
    "514cc3eeaf7608b2",
    "d4300589bfb3064e",
    "63f27822adc1850e",
    "56521e9c584d3d37",
    "518905d3194e03d3",
    "7ab35be1b4027364",
    "40385e745fcd3217",
    "426594f84747fa93",
    "cce7842f454f6039",
    "ec1d8e3053b75675",
    "bbbeb7204f4d8f53",
    "3551ce1bfdad421d",
    "b3258aee445dc73b",
];

/// FNV-1a 64, hex. Same non-cryptographic question as the manifest seed:
/// "did the operator edit this file", not "is this an attack".
fn fnv64(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub(crate) fn unedited_seed(data: &[u8]) -> bool {
    EVER_SHIPPED_FNV64.contains(&fnv64(data).as_str())
}

/// What one [`seed_into`] run did, in the order a caller checks them.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The shipped body was created at [`SeededSkill::path`].
    Written,
    /// A file was already there and was left alone: it may be current,
    /// outdated, or operator-edited.
    Kept,
    /// No consumer for this destination is installed. Nothing was created.
    NoAgent,
    /// Setup could not safely write; the sentence says where and why.
    Problem(String),
}

/// One seed attempt against one canonical consumer destination.
pub struct SeededSkill {
    pub consumer: &'static str,
    pub path: PathBuf,
    pub outcome: Outcome,
}

/// One body-free preview of a consumer's skill destination.
pub struct PlannedSkill {
    pub consumer: &'static str,
    pub path: PathBuf,
    pub decision: crate::managed_assets::SeedDecision,
}

/// One canonical seed target. The target retains its root and relative path
/// so planning and setup can make their ownership decision without
/// path-following I/O.
struct SkillTarget {
    consumer: &'static str,
    installation: Installation,
    installation_roots: Vec<PathBuf>,
    location: crate::consumer::AssetLocation,
    missing_root: MissingRoot,
}

/// The only declared managed root that setup may create after it verifies an
/// installed consumer. The shared Codex/Cursor skill intentionally lives in
/// `~/.agents`, outside either consumer's own root. All other target roots
/// must already exist.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MissingRoot {
    Refuse,
    CreateSharedCodexCursor,
}

/// Whether the consumer root is present through a no-follow inspection.
/// An unsafe root is visible for manual review rather than being mistaken
/// for an absent consumer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Installation {
    Absent,
    Present,
    Unproven,
}

impl Installation {
    fn inspect(root: &Path) -> Self {
        match StateInspector::open_existing(root) {
            Ok(Some(root)) => match root.path_matches_held_root() {
                Ok(true) => Self::Present,
                Ok(false) | Err(_) => Self::Unproven,
            },
            Ok(None) => Self::Absent,
            Err(_) => Self::Unproven,
        }
    }
}

fn combined_installation(roots: &[PathBuf]) -> Installation {
    let mut present = false;
    for root in roots {
        match Installation::inspect(root) {
            Installation::Present => present = true,
            Installation::Unproven => return Installation::Unproven,
            Installation::Absent => {}
        }
    }
    if present {
        Installation::Present
    } else {
        Installation::Absent
    }
}

/// The canonical consumer destinations for this user home. Codex and Cursor
/// share a target when the consumer catalog says they do, so the writer and
/// read-only plan cannot drift into two copies of one skill.
fn targets(home: &Path) -> Vec<SkillTarget> {
    let claude = crate::consumer::spec(crate::hookset::CliKind::Claude);
    let claude_locations = claude.locations(home);
    let codex = crate::consumer::spec(crate::hookset::CliKind::Codex);
    let codex_locations = codex.locations(home);
    let cursor = crate::consumer::spec(crate::hookset::CliKind::Cursor);
    let cursor_locations = cursor.locations(home);
    let agy = crate::consumer::spec(crate::hookset::CliKind::Agy);
    let agy_locations = agy.locations(home);
    let codex_installation = Installation::inspect(&codex_locations.install_root);
    let cursor_installation = Installation::inspect(&cursor_locations.install_root);
    let (shared_consumer, shared_installation) = match (codex_installation, cursor_installation) {
        (Installation::Present, Installation::Present) => {
            ("Codex and Cursor", Installation::Present)
        }
        (Installation::Present, Installation::Absent) => (codex.skill_name, Installation::Present),
        (Installation::Absent, Installation::Present) => (cursor.skill_name, Installation::Present),
        (Installation::Absent, Installation::Absent) => ("Codex and Cursor", Installation::Absent),
        _ => ("Codex and Cursor", Installation::Unproven),
    };
    let mut targets = vec![SkillTarget {
        consumer: claude.skill_name,
        installation: Installation::inspect(&claude_locations.install_root),
        installation_roots: vec![claude_locations.install_root.clone()],
        location: claude_locations.skill,
        missing_root: MissingRoot::Refuse,
    }];
    if codex_locations.skill == cursor_locations.skill {
        targets.push(SkillTarget {
            consumer: shared_consumer,
            installation: shared_installation,
            installation_roots: vec![
                codex_locations.install_root.clone(),
                cursor_locations.install_root.clone(),
            ],
            location: codex_locations.skill,
            missing_root: MissingRoot::CreateSharedCodexCursor,
        });
    } else {
        targets.push(SkillTarget {
            consumer: codex.skill_name,
            installation: codex_installation,
            installation_roots: vec![codex_locations.install_root.clone()],
            location: codex_locations.skill,
            missing_root: MissingRoot::Refuse,
        });
        targets.push(SkillTarget {
            consumer: cursor.skill_name,
            installation: cursor_installation,
            installation_roots: vec![cursor_locations.install_root.clone()],
            location: cursor_locations.skill,
            missing_root: MissingRoot::Refuse,
        });
    }
    targets.push(SkillTarget {
        consumer: agy.skill_name,
        installation: Installation::inspect(&agy_locations.install_root),
        installation_roots: vec![agy_locations.install_root.clone()],
        location: agy_locations.skill,
        missing_root: MissingRoot::Refuse,
    });
    targets
}

/// Preview installed consumers and name an unproven consumer root for manual
/// review. A shared skill directory by itself is not evidence that Codex or
/// Cursor is installed, so it never becomes a plan target on its own.
pub fn plan(home: &Path) -> Vec<PlannedSkill> {
    targets(home)
        .into_iter()
        .filter(|target| target.installation != Installation::Absent)
        .map(|target| PlannedSkill {
            consumer: target.consumer,
            decision: seed_decision_at(target.installation, target.missing_root, &target.location),
            path: target.location.path(),
        })
        .collect()
}

/// Read one target through a held no-follow descriptor. Links, multi-link
/// files, failed reads, and an unproven consumer root cannot establish a
/// safe create decision, so they remain untouched.
fn seed_decision_at(
    installation: Installation,
    missing_root: MissingRoot,
    location: &crate::consumer::AssetLocation,
) -> crate::managed_assets::SeedDecision {
    if installation != Installation::Present {
        return crate::managed_assets::refuse_unreadable_or_unproven();
    }
    match StateInspector::open_existing(&location.root) {
        Ok(Some(root)) => inspected_seed_decision(&root, location)
            .unwrap_or_else(|()| crate::managed_assets::refuse_unreadable_or_unproven()),
        // Codex and Cursor store their shared skill below `.agents`, which
        // may be absent even when one of those consumers is installed. No
        // direct consumer root is treated as creatable here.
        Ok(None) if missing_root == MissingRoot::CreateSharedCodexCursor => {
            crate::managed_assets::seed_decision(None, SHIPPED.as_bytes(), unedited_seed)
        }
        Ok(None) => crate::managed_assets::refuse_unreadable_or_unproven(),
        Err(_) => crate::managed_assets::refuse_unreadable_or_unproven(),
    }
}

/// Read one target only through the root descriptor that will later be
/// transferred into managed publication authority. The resulting decision
/// never authorizes changing an existing leaf.
fn inspected_seed_decision(
    root: &StateInspector,
    location: &crate::consumer::AssetLocation,
) -> Result<crate::managed_assets::SeedDecision, ()> {
    match root.read_file(
        &location.relative,
        cyclops_state::INSPECTION_FILE_BYTES_LIMIT_MAX,
    ) {
        Ok(Some(file)) if !file.truncated => Ok(crate::managed_assets::seed_decision(
            Some(&file.bytes),
            SHIPPED.as_bytes(),
            unedited_seed,
        )),
        Ok(None) => Ok(crate::managed_assets::seed_decision(
            None,
            SHIPPED.as_bytes(),
            unedited_seed,
        )),
        Ok(Some(_)) | Err(_) => Err(()),
    }
}

fn manual_review(path: &Path, cause: impl std::fmt::Display) -> Outcome {
    Outcome::Problem(format!(
        "manual review required for {}: {cause}; left untouched",
        path.display()
    ))
}

fn publish_problem(path: &Path, error: StateError) -> Outcome {
    match error {
        StateError::CreationDurabilityUnknown { .. } => Outcome::Problem(format!(
            "manual review required for {}: publication may be visible, but durability is unknown; inspect it before retrying",
            path.display()
        )),
        error => manual_review(path, error),
    }
}

/// Create the shipped skill only when its declared destination is missing.
///
/// The installation proof is repeated immediately before each mutation. A
/// direct consumer root that disappeared after target selection is never
/// recreated. The one explicit exception is the shared Codex/Cursor
/// `.agents` destination, which may be created only while one of those
/// consumers still proves installed.
fn seed_into(
    consumer: &'static str,
    installation_roots: &[PathBuf],
    missing_root: MissingRoot,
    location: crate::consumer::AssetLocation,
) -> SeededSkill {
    let path = location.path();
    let installation = combined_installation(installation_roots);
    if installation == Installation::Absent {
        return SeededSkill {
            consumer,
            path,
            outcome: Outcome::NoAgent,
        };
    }
    if installation == Installation::Unproven {
        return SeededSkill {
            consumer,
            path: path.clone(),
            outcome: manual_review(&path, "consumer root is unproven"),
        };
    }

    // Keep the descriptor that classified an existing target. Publishing a
    // missing leaf remains below this same no-follow boundary.
    let root = match StateInspector::open_existing(&location.root) {
        Ok(Some(root)) => root,
        Ok(None) => {
            if missing_root != MissingRoot::CreateSharedCodexCursor {
                return SeededSkill {
                    consumer,
                    path: path.clone(),
                    outcome: manual_review(
                        &path,
                        "declared managed root is absent; setup will not recreate a consumer root",
                    ),
                };
            }
            match combined_installation(installation_roots) {
                Installation::Present => {}
                Installation::Absent => {
                    return SeededSkill {
                        consumer,
                        path,
                        outcome: Outcome::NoAgent,
                    };
                }
                Installation::Unproven => {
                    return SeededSkill {
                        consumer,
                        path: path.clone(),
                        outcome: manual_review(&path, "consumer root is unproven"),
                    };
                }
            }
            let authority = match ManagedAssetRoot::open_or_create(&location.root) {
                Ok(authority) => authority,
                Err(error) => {
                    return SeededSkill {
                        consumer,
                        path: path.clone(),
                        outcome: publish_problem(&path, error),
                    };
                }
            };
            let outcome = match authority.create_file_once(&location.relative, SHIPPED.as_bytes()) {
                Ok(CreateFileOutcome::Created) => Outcome::Written,
                Ok(CreateFileOutcome::AlreadyExists) => {
                    manual_review(&path, "target appeared while the skill was being published")
                }
                Err(error) => publish_problem(&path, error),
            };
            return SeededSkill {
                consumer,
                path,
                outcome,
            };
        }
        Err(error) => {
            return SeededSkill {
                consumer,
                path: path.clone(),
                outcome: manual_review(&path, error),
            };
        }
    };
    let decision = match inspected_seed_decision(&root, &location) {
        Ok(inspection) => inspection,
        Err(()) => {
            return SeededSkill {
                consumer,
                path: path.clone(),
                outcome: manual_review(&path, "target cannot be safely inspected"),
            };
        }
    };
    match decision.action() {
        crate::managed_assets::SeedAction::KeepCurrent
        | crate::managed_assets::SeedAction::PreserveKnownOldSeed
        | crate::managed_assets::SeedAction::PreserveOperatorEdit => {
            return SeededSkill {
                consumer,
                path,
                outcome: Outcome::Kept,
            };
        }
        crate::managed_assets::SeedAction::RefuseUnreadableOrUnproven => {
            return SeededSkill {
                consumer,
                path: path.clone(),
                outcome: manual_review(&path, decision.ownership_reason()),
            };
        }
        crate::managed_assets::SeedAction::Create => {}
    }
    let authority = root.into_managed_asset_root();
    let outcome = match authority.create_file_once(&location.relative, SHIPPED.as_bytes()) {
        Ok(CreateFileOutcome::Created) => Outcome::Written,
        Ok(CreateFileOutcome::AlreadyExists) => {
            manual_review(&path, "target appeared while the skill was being published")
        }
        Err(error) => publish_problem(&path, error),
    };
    SeededSkill {
        consumer,
        path,
        outcome,
    }
}

/// Seed canonical destinations only when their consumer homes exist.
pub fn seed() -> Vec<SeededSkill> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    targets(&home)
        .into_iter()
        .map(|target| {
            seed_into(
                target.consumer,
                &target.installation_roots,
                target.missing_root,
                target.location,
            )
        })
        .collect()
}

/// The note `cyclops start --setup-only` prints for one seed attempt.
/// Only a write or a failure speaks: a kept file and an absent agent CLI
/// are both the ordinary case on a rerun, and a note repeated on every
/// setup is noise.
pub fn note(seeded: &SeededSkill) -> Option<String> {
    match &seeded.outcome {
        Outcome::Written => Some(format!(
            "installed the {} skill at {}",
            seeded.consumer,
            seeded.path.display()
        )),
        Outcome::Problem(cause) => Some(format!("skill: {cause}")),
        Outcome::Kept | Outcome::NoAgent => None,
    }
}

/// The stable word `cyclops start --setup-only --json` prints for an
/// outcome. The cause of a `Problem` travels in the note, not here.
pub fn json_word(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Written => "written",
        Outcome::Kept => "kept",
        Outcome::NoAgent => "no_agent",
        Outcome::Problem(_) => "problem",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The compiled-in copy has to be the file in the repo, and the file
    /// has to carry the frontmatter an agent skill loader looks for. A
    /// binary shipping a skill that drifted from the repo would install
    /// instructions nobody reviews.
    #[test]
    fn the_shipped_skill_is_the_repo_file_and_names_itself() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/cyclops/SKILL.md");
        let text = std::fs::read_to_string(&repo).expect("the repo skill file");
        assert_eq!(text, SHIPPED, "the binary's skill is not the repo's");
        assert!(
            SHIPPED.starts_with("---\nname: cyclops\n"),
            "the frontmatter is what a skill loader keys on"
        );
        // Installed far from the repo, a repo-relative link is a dangling
        // path. Every doc link must stand alone.
        assert!(
            !SHIPPED.contains("../../docs"),
            "the skill carries repo-relative links that dangle once installed"
        );
    }

    /// Same classification contract as the manifest list: whoever changes
    /// the skill sees this fail and appends the new hash, so an older release
    /// remains recognizable as outdated rather than edited.
    #[test]
    fn the_ever_shipped_list_contains_the_current_body() {
        assert!(
            EVER_SHIPPED_FNV64.contains(&fnv64(SHIPPED.as_bytes()).as_str()),
            "the current skill body is not in EVER_SHIPPED_FNV64; add {}",
            fnv64(SHIPPED.as_bytes())
        );
    }

    /// The whole policy in one run: a missing direct consumer root is not
    /// recreated, a fresh seed writes below a present root, and existing
    /// current or operator-owned bytes stay untouched.
    #[test]
    fn the_seed_respects_the_vendor_dir_and_the_operators_edits() {
        let root = cyclops_proto::scratch::scratch_dir("cyc-skillseed");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        let agent_dir = root.join(".claude");
        let skill = crate::consumer::AssetLocation {
            root: agent_dir.clone(),
            relative: PathBuf::from("skills/cyclops/SKILL.md"),
        };

        // Not installed: nothing appears, not even the directory.
        let installation_roots = vec![agent_dir.clone()];
        let absent = seed_into(
            "test agent",
            &installation_roots,
            MissingRoot::Refuse,
            skill.clone(),
        );
        assert_eq!(absent.outcome, Outcome::NoAgent);
        assert!(!agent_dir.exists(), "seeding invented the vendor dir");
        assert_eq!(note(&absent), None);

        // Installed: the skill lands and says so.
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        let first = seed_into(
            "test agent",
            &installation_roots,
            MissingRoot::Refuse,
            skill.clone(),
        );
        assert_eq!(first.outcome, Outcome::Written);
        assert_eq!(
            std::fs::read_to_string(&first.path).expect("written"),
            SHIPPED
        );
        assert!(note(&first)
            .expect("a write speaks")
            .contains("installed the test agent skill"));

        // Rerun: current file, no write, no note.
        let second = seed_into(
            "test agent",
            &installation_roots,
            MissingRoot::Refuse,
            skill.clone(),
        );
        assert_eq!(second.outcome, Outcome::Kept);
        assert_eq!(note(&second), None);

        // An operator edit outranks the shipped copy on every later run.
        std::fs::write(&first.path, "# my own notes\n").expect("edit");
        let third = seed_into(
            "test agent",
            &installation_roots,
            MissingRoot::Refuse,
            skill,
        );
        assert_eq!(third.outcome, Outcome::Kept);
        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            "# my own notes\n"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_direct_consumer_root_removed_after_target_selection_is_not_recreated() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-skillseed-root-removed");
        let _ = std::fs::remove_dir_all(&home);
        let claude_root = home.join(".claude");
        std::fs::create_dir_all(&claude_root).expect("create Claude root");

        let target = targets(&home)
            .into_iter()
            .find(|target| target.consumer == "Claude Code")
            .expect("Claude target");
        assert!(target.installation == Installation::Present);
        std::fs::remove_dir_all(&claude_root).expect("remove selected consumer root");

        let seeded = seed_into(
            target.consumer,
            &target.installation_roots,
            target.missing_root,
            target.location,
        );
        assert_eq!(seeded.outcome, Outcome::NoAgent);
        assert!(
            !claude_root.exists(),
            "setup recreated a direct consumer root after its proof disappeared"
        );

        let _ = std::fs::remove_dir_all(&home);
    }
}
