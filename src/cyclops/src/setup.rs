//! Read-only setup inspection for every shipped agent consumer.

use std::path::{Path, PathBuf};

use cyclops_state::{StateError, StateInspector, INSPECTION_FILE_BYTES_LIMIT_MAX};
use serde_json::json;

use crate::copy;
use crate::style::Style;

#[derive(Clone, Copy)]
enum FileState {
    Missing,
    Shipped(crate::managed_assets::ShippedState),
    Invalid,
    Unreadable,
    ManualReview,
}

impl FileState {
    fn word(self) -> &'static str {
        match self {
            FileState::Missing => "missing",
            FileState::Shipped(state) => state.word(),
            FileState::Invalid => "invalid",
            FileState::Unreadable => "unreadable",
            FileState::ManualReview => "manual_review_required",
        }
    }

    fn ready(self) -> bool {
        matches!(self, FileState::Shipped(state) if state.ready())
    }
}

/// A file result obtained only through a held, no-follow descriptor.
///
/// `ManualReview` is reserved for a link, ownership, hard-link, or other
/// unsafe boundary. An ordinary read failure keeps the older `unreadable`
/// machine contract without implying that Cyclops followed anything.
enum AssetRead {
    Missing,
    Bytes(Vec<u8>),
    Unreadable,
    ManualReview,
}

fn asset_read_error(error: StateError) -> AssetRead {
    match error {
        StateError::UnsafePath { .. } => AssetRead::ManualReview,
        StateError::Io { .. }
        | StateError::ReplacementDurabilityUnknown { .. }
        | StateError::CreationDurabilityUnknown { .. }
        | StateError::CreationMayBeVisible { .. }
        | StateError::RemovalDurabilityUnknown { .. } => AssetRead::Unreadable,
    }
}

fn read_asset(root: &Path, relative: &Path) -> AssetRead {
    match StateInspector::open_existing(root) {
        Ok(Some(inspector)) => read_asset_from(&inspector, relative),
        Ok(None) => AssetRead::Missing,
        Err(error) => asset_read_error(error),
    }
}

fn read_asset_from(inspector: &StateInspector, relative: &Path) -> AssetRead {
    let asset = match inspector.read_file(relative, INSPECTION_FILE_BYTES_LIMIT_MAX) {
        Ok(Some(file)) if file.truncated => AssetRead::Unreadable,
        Ok(Some(file)) => AssetRead::Bytes(file.bytes),
        Ok(None) => AssetRead::Missing,
        Err(error) => asset_read_error(error),
    };
    match inspector.path_matches_held_root() {
        Ok(true) => asset,
        Ok(false) | Err(_) => AssetRead::ManualReview,
    }
}

fn read_skill_asset(location: &crate::consumer::AssetLocation) -> AssetRead {
    match crate::skillseed::inspect(location) {
        crate::skillseed::SkillInspection::Missing => AssetRead::Missing,
        crate::skillseed::SkillInspection::Bytes(bytes) => AssetRead::Bytes(bytes),
        crate::skillseed::SkillInspection::Unreadable => AssetRead::Unreadable,
        crate::skillseed::SkillInspection::ManualReview => AssetRead::ManualReview,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Installation {
    Absent,
    Present,
    ManualReview,
}

impl Installation {
    fn inspect(root: &Path) -> Self {
        match StateInspector::open_existing(root) {
            Ok(Some(inspector)) => match inspector.path_matches_held_root() {
                Ok(true) => Self::Present,
                Ok(false) | Err(_) => Self::ManualReview,
            },
            Ok(None) => Self::Absent,
            Err(_) => Self::ManualReview,
        }
    }

    fn installed(self) -> bool {
        self != Self::Absent
    }

    fn word(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Present => "present",
            Self::ManualReview => "manual_review_required",
        }
    }
}

struct ManifestCheck {
    path: PathBuf,
    state: FileState,
    ack_capable: bool,
    mailbox_capability_file: Option<PathBuf>,
}

struct ConsumerCheck {
    id: &'static str,
    name: &'static str,
    installation: Installation,
    installed: bool,
    manifest: ManifestCheck,
    hook_path: Option<PathBuf>,
    hook_state: &'static str,
    hook_ready: bool,
    required_receipt: Option<crate::consumer::ReceiptRequirement>,
    skill_path: PathBuf,
    skill_state: &'static str,
    skill_ready: bool,
    mailbox_capability_path: Option<PathBuf>,
    mailbox_capability_ready: Option<bool>,
}

/// One setup-owned seed target, rendered without its file body.
struct PlannedAsset {
    kind: &'static str,
    consumer: Option<&'static str>,
    target: PathBuf,
    decision: crate::managed_assets::SeedDecision,
}

fn planned_assets(cyclops_home: &Path, user_home: &Path) -> Vec<PlannedAsset> {
    let mut assets: Vec<PlannedAsset> = crate::manifests::plan(cyclops_home)
        .into_iter()
        .map(|manifest| PlannedAsset {
            kind: "manifest",
            consumer: None,
            target: manifest.path,
            decision: manifest.decision,
        })
        .collect();
    assets.extend(
        crate::skillseed::plan(user_home)
            .into_iter()
            .map(|skill| PlannedAsset {
                kind: "skill",
                consumer: Some(skill.consumer),
                target: skill.path,
                decision: skill.decision,
            }),
    );
    assets
}

impl ConsumerCheck {
    fn receipt_ready(&self) -> bool {
        match self.installation {
            Installation::Absent => true,
            Installation::Present => self
                .required_receipt
                .is_some_and(|requirement| requirement.accepts(Some(self.manifest.ack_capable))),
            Installation::ManualReview => false,
        }
    }

    fn complete(&self) -> bool {
        self.manifest.state.ready()
            && match self.installation {
                Installation::Absent => true,
                Installation::Present => {
                    self.hook_ready && self.skill_ready && self.receipt_ready()
                }
                Installation::ManualReview => false,
            }
    }
}

fn manifest_check(home: &Path, id: &str) -> ManifestCheck {
    let path = crate::manifests::dir(home).join(format!("{id}.toml"));
    let shipped = crate::manifests::shipped_body(id).expect("shipped consumer manifest");
    let relative = Path::new("manifests").join(format!("{id}.toml"));
    let body = match read_asset(home, &relative) {
        AssetRead::Missing => {
            return ManifestCheck {
                path,
                state: FileState::Missing,
                ack_capable: false,
                mailbox_capability_file: None,
            };
        }
        AssetRead::Unreadable => {
            return ManifestCheck {
                path,
                state: FileState::Unreadable,
                ack_capable: false,
                mailbox_capability_file: None,
            };
        }
        AssetRead::ManualReview => {
            return ManifestCheck {
                path,
                state: FileState::ManualReview,
                ack_capable: false,
                mailbox_capability_file: None,
            };
        }
        AssetRead::Bytes(body) => body,
    };
    let Ok(body) = std::str::from_utf8(&body) else {
        return ManifestCheck {
            path,
            state: FileState::Invalid,
            ack_capable: false,
            mailbox_capability_file: None,
        };
    };
    let parsed = match cyclops_manifest::Manifest::parse(body, &path) {
        Ok(parsed) if parsed.agent.id == id => parsed,
        _ => {
            return ManifestCheck {
                path,
                state: FileState::Invalid,
                ack_capable: false,
                mailbox_capability_file: None,
            };
        }
    };
    let state = FileState::Shipped(crate::managed_assets::classify_seeded_bytes(
        body.as_bytes(),
        shipped.as_bytes(),
        crate::manifests::unedited_seed,
    ));
    ManifestCheck {
        path,
        state,
        ack_capable: parsed.hooks.ack.is_some(),
        mailbox_capability_file: parsed.messaging.mailbox_capability_file,
    }
}

fn skill_state(installation: Installation, asset: AssetRead) -> (&'static str, bool) {
    match installation {
        Installation::Absent => ("not_installed", true),
        Installation::ManualReview => ("manual_review_required", false),
        Installation::Present => match asset {
            AssetRead::Missing => ("missing", false),
            AssetRead::Unreadable => ("unreadable", false),
            AssetRead::ManualReview => ("manual_review_required", false),
            AssetRead::Bytes(body) => {
                let state = crate::managed_assets::classify_seeded_bytes(
                    &body,
                    crate::skillseed::SHIPPED.as_bytes(),
                    crate::skillseed::unedited_seed,
                );
                (state.word(), state.ready())
            }
        },
    }
}

/// Runtime doorbell capability and managed-skill ownership answer different
/// questions. The daemon validates exact regular bytes without following the
/// final link; managed writes additionally require a private, stable parent.
fn mailbox_capability_ready(installed: bool, capability_path: Option<&Path>) -> Option<bool> {
    installed.then(|| capability_path.is_some_and(cyclops_manifest::mailbox_capability::is_current))
}

fn hook_state(
    installation: Installation,
    kind: crate::hookset::CliKind,
    asset: AssetRead,
) -> (&'static str, bool) {
    match installation {
        Installation::Absent => ("not_installed", true),
        Installation::ManualReview => ("manual_review_required", false),
        Installation::Present => match asset {
            AssetRead::Missing => ("missing", false),
            AssetRead::Unreadable => ("unreadable", false),
            AssetRead::ManualReview => ("manual_review_required", false),
            AssetRead::Bytes(bytes) => {
                let state = crate::hookset::inspect_wiring_bytes(kind, &bytes);
                (state.word(), state.ready())
            }
        },
    }
}

fn consumer_check(
    cyclops_home: &Path,
    user_home: &Path,
    spec: &crate::consumer::Spec,
) -> ConsumerCheck {
    let locations = spec.locations(user_home);
    let installation = Installation::inspect(&locations.install_root);
    let installed = installation.installed();
    let manifest = manifest_check(cyclops_home, spec.id);
    let hook_path = locations.hook.path();
    let (hook_state, hook_ready) = hook_state(
        installation,
        spec.kind,
        if installation == Installation::Present {
            read_asset(&locations.hook.root, &locations.hook.relative)
        } else {
            AssetRead::Missing
        },
    );
    let required_receipt = installed.then_some(spec.receipt);
    let skill_path = locations.skill.path();
    let (skill_state, skill_ready) = skill_state(
        installation,
        if installation == Installation::Present {
            read_skill_asset(&locations.skill)
        } else {
            AssetRead::Missing
        },
    );
    let mailbox_capability_path = manifest
        .mailbox_capability_file
        .as_deref()
        .and_then(|path| cyclops_manifest::mailbox_capability::resolve_path(path, user_home));
    let mailbox_capability_ready =
        mailbox_capability_ready(installed, mailbox_capability_path.as_deref());
    ConsumerCheck {
        id: spec.id,
        name: spec.name,
        installation,
        installed,
        manifest,
        hook_path: Some(hook_path),
        hook_state,
        hook_ready,
        required_receipt,
        skill_path,
        skill_state,
        skill_ready,
        mailbox_capability_path,
        mailbox_capability_ready,
    }
}

fn human_state(word: &str) -> String {
    word.replace('_', " ")
}

pub fn run_check(json_out: bool, style: &Style) -> i32 {
    let Some(user_home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("{}", copy::SETUP_HOME_UNAVAILABLE);
        return 1;
    };
    let cyclops_home = cyclops_proto::cyclops_home();
    let checks: Vec<ConsumerCheck> = crate::consumer::SHIPPED
        .iter()
        .map(|spec| consumer_check(&cyclops_home, &user_home, spec))
        .collect();
    let complete = checks.iter().all(ConsumerCheck::complete);

    if json_out {
        println!(
            "{}",
            json!({
                "home": cyclops_home.display().to_string(),
                "complete": complete,
                "consumers": checks.iter().map(|check| json!({
                    "id": check.id,
                    "name": check.name,
                    "installed": check.installed,
                    "install_state": check.installation.word(),
                    "manifest": {
                        "path": check.manifest.path.display().to_string(),
                        "state": check.manifest.state.word(),
                    },
                    "hook": {
                        "path": check.hook_path.as_ref().map(|path| path.display().to_string()),
                        "state": check.hook_state,
                        "required_receipt_tier": check.required_receipt.map(|requirement| requirement.tier()),
                        "ack_capable": check.installed.then_some(check.manifest.ack_capable),
                        "receipt_ready": check.installed.then(|| check.receipt_ready()),
                    },
                    "skill": {
                        "path": check.skill_path.display().to_string(),
                        "state": check.skill_state,
                    },
                    "mailbox": {
                        "capability_path": check.mailbox_capability_path.as_ref().map(|path| path.display().to_string()),
                        "doorbell_ready": check.mailbox_capability_ready,
                        "transport": check.mailbox_capability_ready.map(|ready| if ready { "doorbell" } else { "direct_payload" }),
                    },
                })).collect::<Vec<_>>(),
            })
        );
        return i32::from(!complete);
    }

    let heading = if complete {
        "✔ setup complete"
    } else {
        "⚠ setup incomplete"
    };
    println!("{}", style.bold(heading));
    for check in &checks {
        let installed = match check.installation {
            Installation::Present => "installed",
            Installation::Absent => "not installed",
            Installation::ManualReview => "manual review required",
        };
        println!("  {} · {installed}", check.name);
        println!(
            "    manifest  {:<13} {}",
            human_state(check.manifest.state.word()),
            style.dim(&check.manifest.path.display().to_string())
        );
        let receipt = match (
            check.required_receipt.map(|requirement| requirement.tier()),
            check.manifest.ack_capable,
        ) {
            (Some(1), true) => "required tier 1 · ack capable".to_string(),
            (Some(1), false) => "required tier 1 · ack missing".to_string(),
            (Some(tier), _) => format!("required tier {tier}"),
            (None, _) => String::new(),
        };
        let hook_detail = match (&check.hook_path, receipt.is_empty()) {
            (Some(path), false) => format!("{} · {receipt}", path.display()),
            (Some(path), true) => path.display().to_string(),
            (None, false) => receipt,
            (None, true) => "no fixed file".to_string(),
        };
        println!(
            "    hooks     {:<13} {}",
            human_state(check.hook_state),
            style.dim(&hook_detail)
        );
        println!(
            "    skill     {:<13} {}",
            human_state(check.skill_state),
            style.dim(&check.skill_path.display().to_string())
        );
        let (mailbox_state, mailbox_detail) = match check.mailbox_capability_ready {
            Some(true) => ("doorbell", "exact claim skill".to_string()),
            Some(false) => (
                "direct payload",
                check
                    .mailbox_capability_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "manifest has no capability path".to_string()),
            ),
            None => ("not installed", "no target".to_string()),
        };
        println!(
            "    mailbox   {:<13} {}",
            mailbox_state,
            style.dim(&mailbox_detail)
        );
    }
    if !complete {
        println!();
        println!(
            "  {}",
            style.dim("Run cyclops start --setup-only --wire-hooks, then check again.")
        );
    }
    i32::from(!complete)
}

/// Report only the safe setup-owned seeded-file decisions.
///
/// Config, hook wiring, themes, sounds, binaries, cleanup, and uninstall each
/// retain their own lifecycle owners. This narrow preview is deliberately not
/// a dry-run for every setup effect.
pub fn run_plan(json_out: bool, style: &Style) -> i32 {
    let Some(user_home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("setup plan needs a user home to locate installed consumers.");
        return 1;
    };
    let cyclops_home = cyclops_proto::cyclops_home();
    let assets = planned_assets(&cyclops_home, &user_home);

    if json_out {
        println!(
            "{}",
            json!({
                "read_only": true,
                "scope": "managed_asset_decisions_only",
                "apply_available": false,
                "assets": assets.iter().map(|asset| json!({
                    "kind": asset.kind,
                    "consumer": asset.consumer,
                    "target": asset.target.display().to_string(),
                    "observed_state": asset.decision.observed().word(),
                    "action": asset.decision.action().word(),
                    "ownership_reason": asset.decision.ownership_reason(),
                })).collect::<Vec<_>>(),
            })
        );
        return 0;
    }

    println!("{}", style.bold("setup plan · read-only"));
    println!("  {}", style.dim("Managed asset decisions only."));
    println!("  {}", style.dim("No apply command is available yet."));
    for asset in &assets {
        let label = match asset.consumer {
            Some(consumer) => format!("{} · {consumer}", asset.kind),
            None => asset.kind.to_string(),
        };
        println!("  {label}");
        println!(
            "    target    {}",
            style.dim(&asset.target.display().to_string())
        );
        println!(
            "    observed  {}",
            asset.decision.observed().word().replace('_', " ")
        );
        println!(
            "    action    {}",
            asset.decision.action().word().replace('_', " ")
        );
        println!("    ownership {}", asset.decision.ownership_reason());
    }
    println!();
    println!(
        "  {}",
        style.dim("No files were changed. This report has no apply capability yet.")
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_current_doorbell_skill_does_not_need_a_private_parent_to_report_runtime_capability() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("skills/cyclops");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let skill = parent.join("SKILL.md");
        std::fs::write(&skill, crate::skillseed::SHIPPED).unwrap();
        let location = crate::consumer::AssetLocation {
            root: root.path().to_path_buf(),
            relative: PathBuf::from("skills/cyclops/SKILL.md"),
        };

        assert!(matches!(
            read_skill_asset(&location),
            AssetRead::ManualReview
        ));
        assert!(cyclops_manifest::mailbox_capability::is_current(&skill));
        assert_eq!(mailbox_capability_ready(true, Some(&skill)), Some(true));
    }

    #[test]
    fn only_current_or_edited_owned_files_are_ready() {
        assert!(!FileState::Missing.ready());
        assert!(FileState::Shipped(crate::managed_assets::ShippedState::Current).ready());
        assert!(!FileState::Shipped(crate::managed_assets::ShippedState::KnownOld).ready());
        assert!(FileState::Shipped(crate::managed_assets::ShippedState::OperatorEdited).ready());
        assert!(!FileState::Invalid.ready());
        assert!(!FileState::Unreadable.ready());
    }
}
