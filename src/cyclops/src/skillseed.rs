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
//! honored by a later boot: `workspace::finish_deferred_wiring`), only inside
//! an installed consumer's user-owned, non-writable-by-others directory,
//! honoring `CYCLOPS_NO_VENDOR_HOOKS`, and creating only Cyclops's missing
//! `skills/cyclops` leaf and shipped skill. Every existing skill, including a
//! known old Cyclops seed, remains untouched.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use cyclops_state::{CreateFileOutcome, StateError, StateInspector};

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
    "e90798f32ed52ec9",
    "2e02eea187f6c256",
    "09725621ce15f9cf",
    "4042d8569cf02694",
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

/// What explicit uninstall did with one canonical Cyclops skill file.
///
/// Uninstall removes only a byte-for-byte known Cyclops seed. A changed file
/// is the operator's instruction, even when it lives at Cyclops's usual path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemovalOutcome {
    Removed,
    Missing,
    Kept,
    Problem(String),
}

/// One skill-cleanup result for installer reporting.
pub(crate) struct RemovedSkill {
    pub consumer: &'static str,
    pub path: PathBuf,
    pub outcome: RemovalOutcome,
}

/// One body-free preview of a consumer's skill destination.
pub struct PlannedSkill {
    pub consumer: &'static str,
    pub path: PathBuf,
    pub decision: crate::managed_assets::SeedDecision,
}

/// Read-only result for a canonical skill target.
///
/// Missing describes only a missing final file beneath an accepted
/// operator-controlled parent. An unsafe parent is manual review, not a
/// create plan.
pub(crate) enum SkillInspection {
    Missing,
    Bytes(Vec<u8>),
    Unreadable,
    ManualReview,
}

/// One canonical seed target. The target retains its root and relative path
/// so planning and setup can make their ownership decision without
/// path-following I/O.
struct SkillTarget {
    consumer: &'static str,
    installation: Installation,
    installation_roots: Vec<PathBuf>,
    location: crate::consumer::AssetLocation,
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
        });
    } else {
        targets.push(SkillTarget {
            consumer: codex.skill_name,
            installation: codex_installation,
            installation_roots: vec![codex_locations.install_root.clone()],
            location: codex_locations.skill,
        });
        targets.push(SkillTarget {
            consumer: cursor.skill_name,
            installation: cursor_installation,
            installation_roots: vec![cursor_locations.install_root.clone()],
            location: cursor_locations.skill,
        });
    }
    targets.push(SkillTarget {
        consumer: agy.skill_name,
        installation: Installation::inspect(&agy_locations.install_root),
        installation_roots: vec![agy_locations.install_root.clone()],
        location: agy_locations.skill,
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
            decision: seed_decision_at(target.installation, &target.location),
            path: target.location.path(),
        })
        .collect()
}

/// Ensure Cyclops's declared skill directory exists below an installed
/// consumer root, then open it through a held no-follow descriptor.
///
/// Consumer roots remain the consumer's responsibility. Once one exists and
/// is controlled by this user, Cyclops may create only its own fixed
/// `skills/cyclops` leaf path. That makes a normal 0755 consumer directory
/// usable without treating it as private state or changing its permissions.
fn accepted_skill_parent(
    location: &crate::consumer::AssetLocation,
    create_missing: bool,
) -> Result<StateInspector, &'static str> {
    // Read-only inspection and removal do not need authority over the whole
    // consumer tree. Open the final parent directly so an ordinary private
    // scratch root under a platform-owned temporary directory is not mistaken
    // for a writable consumer directory.
    if !create_missing {
        let target = location.path();
        let parent_path = system_path_without_macos_var_alias(
            target
                .parent()
                .ok_or("skill target has no parent directory")?,
        );
        let parent = match StateInspector::open_existing(&parent_path) {
            Ok(Some(parent)) => parent,
            Ok(None) => return Err("Cyclops skill directory is missing"),
            Err(_) => return Err("Cyclops skill directory cannot be safely opened"),
        };
        return match parent.owner_controlled_and_stable() {
            Ok(true) => Ok(parent),
            Ok(false) => {
                Err("Cyclops skill directory is not owned by this user or is writable by others")
            }
            Err(_) => Err("Cyclops skill directory cannot be safely verified"),
        };
    }
    let root = system_path_without_macos_var_alias(&location.root);
    let mut parent = match StateInspector::open_existing(&root) {
        Ok(Some(root)) => root,
        Ok(None) => return Err("consumer root is missing"),
        Err(_) => return Err("consumer root cannot be safely opened"),
    };
    if !parent
        .owner_controlled_and_stable()
        .map_err(|_| "consumer root cannot be safely verified")?
    {
        return Err("consumer root is not owned by this user or is writable by others");
    }

    let relative_parent = location
        .relative
        .parent()
        .ok_or("skill target has no parent directory")?;
    for component in relative_parent.components() {
        let std::path::Component::Normal(name) = component else {
            return Err("skill target has an invalid relative parent");
        };
        let child = parent.path().join(name);
        match StateInspector::open_existing(&child) {
            Ok(Some(existing)) => parent = existing,
            Ok(None) => {
                if !create_missing {
                    return Err("Cyclops skill directory is missing");
                }
                match std::fs::create_dir(&child) {
                    Ok(()) => {
                        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700))
                            .map_err(|_| "Cyclops skill directory permissions could not be set")?
                    }
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(_) => return Err("Cyclops skill directory could not be created"),
                }
                parent = StateInspector::open_existing(&child)
                    .map_err(|_| "Cyclops skill directory cannot be safely opened")?
                    .ok_or("Cyclops skill directory disappeared during setup")?;
            }
            Err(_) => return Err("Cyclops skill directory cannot be safely opened"),
        }
        if !parent
            .owner_controlled_and_stable()
            .map_err(|_| "Cyclops skill directory cannot be safely verified")?
        {
            return Err(
                "Cyclops skill directory is not owned by this user or is writable by others",
            );
        }
    }
    Ok(parent)
}

/// macOS exposes `/var` as a system alias for `/private/var`. It is not an
/// operator-controlled consumer link, but descriptor inspection correctly
/// refuses arbitrary links. Normalize only that fixed system alias so a skill
/// in a normal temporary test or installer scratch path remains inspectable.
fn system_path_without_macos_var_alias(path: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Ok(rest) = path.strip_prefix("/var") {
            return Path::new("/private/var").join(rest);
        }
    }
    path.to_path_buf()
}

fn skill_leaf(location: &crate::consumer::AssetLocation) -> Result<&OsStr, ()> {
    location.relative.file_name().ok_or(())
}

/// Inspect one skill only after accepting its operator-controlled parent.
///
/// Setup check uses this same boundary as seeding so a current-looking leaf
/// below a missing, linked, or nonprivate parent never claims healthy setup.
pub(crate) fn inspect(location: &crate::consumer::AssetLocation) -> SkillInspection {
    let parent = match accepted_skill_parent(location, false) {
        Ok(parent) => parent,
        Err(_) => return SkillInspection::ManualReview,
    };
    let leaf = match skill_leaf(location) {
        Ok(leaf) => leaf,
        Err(()) => return SkillInspection::ManualReview,
    };
    let inspection = match parent.read_file(
        Path::new(leaf),
        cyclops_state::INSPECTION_FILE_BYTES_LIMIT_MAX,
    ) {
        Ok(Some(file)) if !file.truncated && skill_file_is_owner_controlled(&file) => {
            SkillInspection::Bytes(file.bytes)
        }
        Ok(Some(_)) => SkillInspection::Unreadable,
        Ok(None) => SkillInspection::Missing,
        Err(StateError::UnsafePath { .. }) => SkillInspection::ManualReview,
        Err(_) => SkillInspection::Unreadable,
    };
    match parent.owner_controlled_and_stable() {
        Ok(true) => inspection,
        Ok(false) | Err(_) => SkillInspection::ManualReview,
    }
}

/// A consumer may choose a readable skill file, but another user must never
/// be able to replace its instructions. `read_file` already proves regular
/// type, ownership, no links, and stable identity through the held parent.
fn skill_file_is_owner_controlled(file: &cyclops_state::FileInspection) -> bool {
    file.entry.mode & 0o022 == 0
}

/// Read one target through its final operator-controlled parent. Links, multi-link files,
/// missing parents, failed reads, and an unproven consumer root cannot
/// establish a safe create decision, so they remain untouched.
fn seed_decision_at(
    installation: Installation,
    location: &crate::consumer::AssetLocation,
) -> crate::managed_assets::SeedDecision {
    if installation != Installation::Present {
        return crate::managed_assets::refuse_unreadable_or_unproven();
    }
    match inspect(location) {
        SkillInspection::Missing => {
            crate::managed_assets::seed_decision(None, SHIPPED.as_bytes(), unedited_seed)
        }
        SkillInspection::Bytes(bytes) => {
            crate::managed_assets::seed_decision(Some(&bytes), SHIPPED.as_bytes(), unedited_seed)
        }
        SkillInspection::Unreadable | SkillInspection::ManualReview => {
            crate::managed_assets::refuse_unreadable_or_unproven()
        }
    }
}

/// Read one target only through the operator-controlled parent descriptor that will later
/// be transferred into managed publication authority. The resulting decision
/// never authorizes changing an existing leaf.
fn inspected_seed_decision(
    parent: &StateInspector,
    location: &crate::consumer::AssetLocation,
) -> Result<crate::managed_assets::SeedDecision, ()> {
    let leaf = skill_leaf(location)?;
    let decision = match parent.read_file(
        Path::new(leaf),
        cyclops_state::INSPECTION_FILE_BYTES_LIMIT_MAX,
    ) {
        Ok(Some(file)) if !file.truncated && skill_file_is_owner_controlled(&file) => {
            Ok(crate::managed_assets::seed_decision(
                Some(&file.bytes),
                SHIPPED.as_bytes(),
                unedited_seed,
            ))
        }
        Ok(None) => Ok(crate::managed_assets::seed_decision(
            None,
            SHIPPED.as_bytes(),
            unedited_seed,
        )),
        Ok(Some(_)) | Err(_) => Err(()),
    }?;
    match parent.owner_controlled_and_stable() {
        Ok(true) => Ok(decision),
        Ok(false) | Err(_) => Err(()),
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
        StateError::CreationDurabilityUnknown { .. } | StateError::CreationMayBeVisible { .. } => Outcome::Problem(format!(
            "manual review required for {}: publication may be visible, but durability is unknown; inspect it before retrying",
            path.display()
        )),
        error => manual_review(path, error),
    }
}

#[cfg(test)]
mod test_sync {
    use std::cell::RefCell;

    thread_local! {
        static BEFORE_FINAL_INSTALLATION_REVALIDATION: RefCell<Option<Box<dyn FnOnce()>>> =
            RefCell::new(None);
    }

    /// Run a test action in the last gap before a managed-skill publication.
    ///
    /// This lets the regression exercise a consumer uninstall at the exact
    /// boundary that production code revalidates, without a guessed delay.
    pub(super) fn before_final_installation_revalidation() {
        BEFORE_FINAL_INSTALLATION_REVALIDATION.with(|action| {
            if let Some(action) = action.borrow_mut().take() {
                action();
            }
        });
    }

    pub(super) fn remove_consumer_before_final_revalidation(action: impl FnOnce() + 'static) {
        BEFORE_FINAL_INSTALLATION_REVALIDATION.with(|slot| {
            assert!(
                slot.borrow().is_none(),
                "test synchronization action is already set"
            );
            *slot.borrow_mut() = Some(Box::new(action));
        });
    }
}

/// Create the shipped skill only when its declared destination is missing.
///
/// The installation proof is repeated immediately before each mutation. The
/// final parent is created only below an installed, operator-controlled
/// consumer root. Setup never creates a consumer root or changes consumer
/// directory permissions.
fn seed_into(
    consumer: &'static str,
    installation_roots: &[PathBuf],
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

    // Keep the operator-controlled parent descriptor that classified the leaf. Publishing
    // never resolves the user path again before the final `O_EXCL` create.
    let parent = match accepted_skill_parent(&location, true) {
        Ok(parent) => parent,
        Err(cause) => {
            return SeededSkill {
                consumer,
                path: path.clone(),
                outcome: manual_review(&path, cause),
            };
        }
    };
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
    let decision = match inspected_seed_decision(&parent, &location) {
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
    let leaf = match skill_leaf(&location) {
        Ok(leaf) => leaf,
        Err(()) => {
            return SeededSkill {
                consumer,
                path: path.clone(),
                outcome: manual_review(&path, "skill target has no final file name"),
            };
        }
    };
    let authority = match parent.into_operator_owned_asset_root() {
        Ok(authority) => authority,
        Err(error) => {
            return SeededSkill {
                consumer,
                path: path.clone(),
                outcome: manual_review(&path, error),
            };
        }
    };

    // A shared `.agents` parent is not proof that Codex or Cursor is still
    // installed. Check the actual consumer roots again at the publication
    // boundary, after accepting the final parent and before `O_EXCL` can
    // create its only leaf.
    #[cfg(test)]
    test_sync::before_final_installation_revalidation();
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
    let outcome = match authority.create_file_once(Path::new(leaf), SHIPPED.as_bytes()) {
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
        .map(|target| seed_into(target.consumer, &target.installation_roots, target.location))
        .collect()
}

/// Remove only unedited, shipped Cyclops skills during explicit uninstall.
///
/// This deliberately leaves consumer directories, unrelated skills, links,
/// unreadable targets, and edited instructions alone. A failed proof is a
/// reportable manual-cleanup case, never permission to delete a user path.
pub(crate) fn remove_owned(home: &Path) -> Vec<RemovedSkill> {
    targets(home)
        .into_iter()
        .map(|target| remove_owned_at(target.consumer, &target.location))
        .collect()
}

fn remove_owned_at(
    consumer: &'static str,
    location: &crate::consumer::AssetLocation,
) -> RemovedSkill {
    let path = location.path();
    let parent = match accepted_skill_parent(location, false) {
        Ok(parent) => parent,
        Err("consumer root is missing") | Err("Cyclops skill directory is missing") => {
            return RemovedSkill {
                consumer,
                path,
                outcome: RemovalOutcome::Missing,
            };
        }
        Err(cause) => {
            return RemovedSkill {
                consumer,
                path: path.clone(),
                outcome: RemovalOutcome::Problem(format!(
                    "manual review required for {}: {cause}; left untouched",
                    path.display()
                )),
            };
        }
    };
    let leaf = match skill_leaf(location) {
        Ok(leaf) => leaf,
        Err(()) => {
            return RemovedSkill {
                consumer,
                path: path.clone(),
                outcome: RemovalOutcome::Problem(format!(
                    "manual review required for {}: target has no final file name; left untouched",
                    path.display()
                )),
            };
        }
    };
    let file = match parent.read_file(
        Path::new(leaf),
        cyclops_state::INSPECTION_FILE_BYTES_LIMIT_MAX,
    ) {
        Ok(None) => {
            return RemovedSkill {
                consumer,
                path,
                outcome: RemovalOutcome::Missing,
            };
        }
        Ok(Some(file))
            if !file.truncated
                && skill_file_is_owner_controlled(&file)
                && unedited_seed(&file.bytes) =>
        {
            file
        }
        Ok(Some(file)) if !file.truncated && skill_file_is_owner_controlled(&file) => {
            return RemovedSkill {
                consumer,
                path,
                outcome: RemovalOutcome::Kept,
            };
        }
        Ok(Some(_)) | Err(_) => {
            return RemovedSkill {
                consumer,
                path: path.clone(),
                outcome: RemovalOutcome::Problem(format!(
                    "manual review required for {}: skill cannot be safely proved as an unedited Cyclops seed; left untouched",
                    path.display()
                )),
            };
        }
    };
    let removal = match parent.bind_regular_file_for_removal(&file.entry) {
        Ok(removal) => removal,
        Err(error) => {
            return RemovedSkill {
                consumer,
                path: path.clone(),
                outcome: RemovalOutcome::Problem(format!(
                    "manual review required for {}: {error}; left untouched",
                    path.display()
                )),
            };
        }
    };
    match removal.remove() {
        Ok(()) => RemovedSkill {
            consumer,
            path,
            outcome: RemovalOutcome::Removed,
        },
        Err(error) => RemovedSkill {
            consumer,
            path: path.clone(),
            outcome: RemovalOutcome::Problem(format!(
                "manual review required for {}: {error}; left untouched",
                path.display()
            )),
        },
    }
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
/// outcome. A `Problem` carries its manual-review detail separately.
pub fn json_word(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Written => "written",
        Outcome::Kept => "kept",
        Outcome::NoAgent => "no_agent",
        Outcome::Problem(_) => "problem",
    }
}

/// The additive manual-review detail for `cyclops start --setup-only --json`.
///
/// Successful and unchanged outcomes have no detail. Callers can therefore
/// keep using [`json_word`] while a person or automation gets the exact reason
/// the result needs manual review.
pub fn json_detail(outcome: &Outcome) -> Option<&str> {
    match outcome {
        Outcome::Problem(detail) => Some(detail),
        Outcome::Written | Outcome::Kept | Outcome::NoAgent => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[test]
    fn explicit_uninstall_removes_only_an_unedited_shipped_skill() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-skillseed-uninstall");
        let _ = std::fs::remove_dir_all(&home);
        let parent = home.join(".claude/skills/cyclops");
        std::fs::create_dir_all(&parent).expect("create Claude skill parent");
        for directory in [
            home.join(".claude"),
            home.join(".claude/skills"),
            parent.clone(),
        ] {
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755))
                .expect("make normal owner-controlled directory");
        }
        let skill = parent.join("SKILL.md");
        std::fs::write(&skill, SHIPPED).expect("write shipped skill");

        let removed = remove_owned(&home);
        let claude = removed
            .iter()
            .find(|outcome| outcome.consumer == "Claude Code")
            .expect("Claude cleanup result");
        assert_eq!(claude.outcome, RemovalOutcome::Removed);
        assert!(!skill.exists(), "unedited Cyclops skill was not removed");

        std::fs::write(&skill, "operator instruction\n").expect("write operator skill");
        let retained = remove_owned(&home);
        let claude = retained
            .iter()
            .find(|outcome| outcome.consumer == "Claude Code")
            .expect("Claude cleanup result");
        assert_eq!(claude.outcome, RemovalOutcome::Kept);
        assert_eq!(
            std::fs::read_to_string(&skill).unwrap(),
            "operator instruction\n"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

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

    /// The whole policy in one run: a missing direct consumer root is never
    /// recreated, an installed consumer receives Cyclops's fixed skill leaf,
    /// and existing current or operator-owned bytes stay untouched.
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
        let absent = seed_into("test agent", &installation_roots, skill.clone());
        assert_eq!(absent.outcome, Outcome::NoAgent);
        assert!(!agent_dir.exists(), "seeding invented the vendor dir");
        assert_eq!(note(&absent), None);

        // An installed consumer receives only Cyclops's fixed skill leaf.
        // Its existing 0755 root is not rewritten or treated as private
        // state, while the created nested directories are owner-only.
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        let first = seed_into("test agent", &installation_roots, skill.clone());
        assert_eq!(first.outcome, Outcome::Written);
        assert!(
            agent_dir.join("skills/cyclops").is_dir(),
            "seeding created Cyclops's declared skill directory"
        );
        assert_eq!(
            std::fs::metadata(agent_dir.join("skills/cyclops"))
                .expect("created parent")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::read_to_string(&first.path).expect("written"),
            SHIPPED
        );
        assert!(note(&first)
            .expect("a write speaks")
            .contains("installed the test agent skill"));

        // Rerun: current file, no write, no note.
        let second = seed_into("test agent", &installation_roots, skill.clone());
        assert_eq!(second.outcome, Outcome::Kept);
        assert_eq!(note(&second), None);

        // An operator edit outranks the shipped copy on every later run.
        std::fs::write(&first.path, "# my own notes\n").expect("edit");
        let third = seed_into("test agent", &installation_roots, skill);
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

        let seeded = seed_into(target.consumer, &target.installation_roots, target.location);
        assert_eq!(seeded.outcome, Outcome::NoAgent);
        assert!(
            !claude_root.exists(),
            "setup recreated a direct consumer root after its proof disappeared"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_shared_skill_is_not_published_after_its_consumer_disappears_at_the_boundary() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-skillseed-shared-root-removed");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create home");
        let codex_root = home.join(".codex");
        let parent = home.join(".agents/skills/cyclops");
        let skill = parent.join("SKILL.md");
        std::fs::create_dir_all(&codex_root).expect("create Codex root");
        std::fs::create_dir_all(&parent).expect("create shared skill parent");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))
            .expect("make shared skill parent private");
        let location = crate::consumer::AssetLocation {
            root: home.join(".agents"),
            relative: PathBuf::from("skills/cyclops/SKILL.md"),
        };
        let installation_roots = vec![codex_root.clone()];

        // The hook is invoked after the shared parent is accepted and just
        // before the last consumer-root check. It is deterministic evidence
        // that an uninstall in that gap cannot publish the shared skill.
        let removed_root = codex_root.clone();
        test_sync::remove_consumer_before_final_revalidation(move || {
            std::fs::remove_dir_all(&removed_root).expect("remove consumer root at boundary");
        });

        let seeded = seed_into("Codex", &installation_roots, location);
        assert_eq!(seeded.outcome, Outcome::NoAgent);
        assert!(!codex_root.exists(), "the synchronized uninstall ran");
        assert!(
            !skill.exists(),
            "setup published SKILL.md after its consumer disappeared"
        );

        let _ = std::fs::remove_dir_all(&home);
    }
}
