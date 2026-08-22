//! Read-only setup inspection for every shipped agent consumer.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::copy;
use crate::hookset::CliKind;
use crate::style::Style;

#[derive(Clone, Copy)]
enum FileState {
    Missing,
    Current,
    Outdated,
    Edited,
    Invalid,
    Unreadable,
}

impl FileState {
    fn word(self) -> &'static str {
        match self {
            FileState::Missing => "missing",
            FileState::Current => "current",
            FileState::Outdated => "outdated",
            FileState::Edited => "edited",
            FileState::Invalid => "invalid",
            FileState::Unreadable => "unreadable",
        }
    }

    fn ready(self) -> bool {
        matches!(self, FileState::Current | FileState::Edited)
    }
}

struct ManifestCheck {
    path: PathBuf,
    state: FileState,
    ack_capable: bool,
    launch_flag_present: bool,
    mailbox_capability_file: Option<PathBuf>,
}

struct ConsumerCheck {
    id: &'static str,
    name: &'static str,
    installed: bool,
    manifest: ManifestCheck,
    hook_path: Option<PathBuf>,
    hook_state: &'static str,
    hook_ready: bool,
    required_receipt_tier: Option<u8>,
    skill_path: PathBuf,
    skill_state: &'static str,
    skill_ready: bool,
    mailbox_capability_path: Option<PathBuf>,
    mailbox_capability_ready: Option<bool>,
}

impl ConsumerCheck {
    fn receipt_ready(&self) -> bool {
        !self.installed || self.required_receipt_tier != Some(1) || self.manifest.ack_capable
    }

    fn complete(&self) -> bool {
        self.manifest.state.ready()
            && (!self.installed || (self.hook_ready && self.skill_ready && self.receipt_ready()))
    }
}

struct Spec {
    id: &'static str,
    name: &'static str,
    kind: CliKind,
    required_receipt_tier: u8,
}

const CONSUMERS: &[Spec] = &[
    Spec {
        id: "claude",
        name: "Claude Code",
        kind: CliKind::Claude,
        required_receipt_tier: 1,
    },
    Spec {
        id: "codex",
        name: "Codex CLI",
        kind: CliKind::Codex,
        required_receipt_tier: 1,
    },
    Spec {
        id: "cursor",
        name: "Cursor Agent CLI",
        kind: CliKind::Cursor,
        required_receipt_tier: 1,
    },
    Spec {
        id: "agy",
        name: "Antigravity CLI",
        kind: CliKind::Agy,
        required_receipt_tier: 2,
    },
];

fn skill_path(home: &Path, id: &str) -> PathBuf {
    match id {
        "claude" => crate::skillseed::skill_path(&home.join(".claude")),
        "codex" | "cursor" => crate::skillseed::skill_path(&home.join(".agents")),
        "agy" => crate::skillseed::skill_path(&home.join(".gemini/antigravity-cli")),
        _ => unreachable!("shipped consumer id"),
    }
}

fn manifest_check(home: &Path, id: &str) -> ManifestCheck {
    let path = crate::manifests::dir(home).join(format!("{id}.toml"));
    let shipped = crate::manifests::shipped_body(id).expect("shipped consumer manifest");
    let Ok(body) = std::fs::read_to_string(&path) else {
        let state = if path.exists() {
            FileState::Unreadable
        } else {
            FileState::Missing
        };
        return ManifestCheck {
            path,
            state,
            ack_capable: false,
            launch_flag_present: false,
            mailbox_capability_file: None,
        };
    };
    let parsed = match cyclops_manifest::Manifest::parse(&body, &path) {
        Ok(parsed) if parsed.agent.id == id => parsed,
        _ => {
            return ManifestCheck {
                path,
                state: FileState::Invalid,
                ack_capable: false,
                launch_flag_present: false,
                mailbox_capability_file: None,
            };
        }
    };
    let state = if body == shipped {
        FileState::Current
    } else if crate::manifests::unedited_seed(body.as_bytes()) {
        FileState::Outdated
    } else {
        FileState::Edited
    };
    ManifestCheck {
        path,
        state,
        ack_capable: parsed.hooks.ack.is_some(),
        launch_flag_present: parsed
            .hooks
            .settings_flag
            .as_deref()
            .is_some_and(|flag| !flag.trim().is_empty()),
        mailbox_capability_file: parsed.messaging.mailbox_capability_file,
    }
}

fn skill_state(installed: bool, path: &Path) -> (&'static str, bool) {
    if !installed {
        return ("not_installed", true);
    }
    match std::fs::read(path) {
        Ok(body) if body == crate::skillseed::SHIPPED.as_bytes() => ("current", true),
        Ok(body) if crate::skillseed::unedited_seed(&body) => ("outdated", false),
        Ok(_) => ("edited", true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ("missing", false),
        Err(_) => ("unreadable", false),
    }
}

fn consumer_check(cyclops_home: &Path, user_home: &Path, spec: &Spec) -> ConsumerCheck {
    let installed = crate::consumer::root(spec.kind, user_home).is_dir();
    let manifest = manifest_check(cyclops_home, spec.id);
    let wiring = crate::hookset::inspect_wiring(spec.kind);
    let (hook_state, hook_ready) = if !installed {
        ("not_installed", true)
    } else if spec.kind == CliKind::Claude && !manifest.launch_flag_present {
        ("missing_launch_flag", false)
    } else {
        (wiring.state.word(), wiring.state.ready())
    };
    let required_receipt_tier = installed.then_some(spec.required_receipt_tier);
    let skill_path = skill_path(user_home, spec.id);
    let (skill_state, skill_ready) = skill_state(installed, &skill_path);
    let mailbox_capability_path = manifest
        .mailbox_capability_file
        .as_deref()
        .and_then(|path| cyclops_manifest::mailbox_capability::resolve_path(path, user_home));
    let mailbox_capability_ready = installed.then(|| {
        mailbox_capability_path
            .as_deref()
            .is_some_and(cyclops_manifest::mailbox_capability::is_current)
    });
    ConsumerCheck {
        id: spec.id,
        name: spec.name,
        installed,
        manifest,
        hook_path: wiring.path,
        hook_state,
        hook_ready,
        required_receipt_tier,
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
    let checks: Vec<ConsumerCheck> = CONSUMERS
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
                    "manifest": {
                        "path": check.manifest.path.display().to_string(),
                        "state": check.manifest.state.word(),
                    },
                    "hook": {
                        "path": check.hook_path.as_ref().map(|path| path.display().to_string()),
                        "state": check.hook_state,
                        "required_receipt_tier": check.required_receipt_tier,
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
        let installed = if check.installed {
            "installed"
        } else {
            "not installed"
        };
        println!("  {} · {installed}", check.name);
        println!(
            "    manifest  {:<13} {}",
            human_state(check.manifest.state.word()),
            style.dim(&check.manifest.path.display().to_string())
        );
        let receipt = match (check.required_receipt_tier, check.manifest.ack_capable) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_current_or_edited_owned_files_are_ready() {
        assert!(!FileState::Missing.ready());
        assert!(FileState::Current.ready());
        assert!(!FileState::Outdated.ready());
        assert!(FileState::Edited.ready());
        assert!(!FileState::Invalid.ready());
        assert!(!FileState::Unreadable.ready());
    }

    #[test]
    fn wiring_state_words_are_stable_for_the_report() {
        assert_eq!(crate::hookset::WiringState::OnLaunch.word(), "on_launch");
    }
}
