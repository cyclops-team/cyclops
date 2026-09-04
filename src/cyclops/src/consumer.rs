//! Immutable setup facts for the agent consumers Cyclops ships.
//!
//! Runtime observation decides which exact process occupies a pane. Hook
//! wiring and skill seeding own their filesystem effects. This module owns
//! the vendor catalog those callers share: names, receipt requirements, and
//! canonical install, hook, and skill locations.

use std::path::{Path, PathBuf};

use crate::hookset::CliKind;

/// The receipt strength an installed consumer must be able to provide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptRequirement {
    /// A hook payload can identify the exact staged message.
    ExactHook = 1,
    /// Only screen evidence can identify the staged message.
    Screen = 2,
}

impl ReceiptRequirement {
    pub(crate) fn tier(self) -> u8 {
        self as u8
    }

    /// Whether the parsed manifest satisfies this consumer's receipt floor.
    pub(crate) fn accepts(self, ack_capable: Option<bool>) -> bool {
        self == Self::Screen || ack_capable == Some(true)
    }
}

/// One path inspected by setup and health without changing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AssetLocation {
    pub(crate) root: PathBuf,
    pub(crate) relative: PathBuf,
}

impl AssetLocation {
    pub(crate) fn path(&self) -> PathBuf {
        self.root.join(&self.relative)
    }
}

/// Canonical paths for one consumer on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Locations {
    pub(crate) install_root: PathBuf,
    pub(crate) hook: AssetLocation,
    pub(crate) skill: AssetLocation,
}

/// Vendor facts shared by setup, health, and hook wiring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Spec {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) skill_name: &'static str,
    pub(crate) kind: CliKind,
    pub(crate) receipt: ReceiptRequirement,
}

pub(crate) const SHIPPED: &[Spec] = &[
    Spec {
        id: "claude",
        name: "Claude Code",
        skill_name: "Claude Code",
        kind: CliKind::Claude,
        receipt: ReceiptRequirement::ExactHook,
    },
    Spec {
        id: "codex",
        name: "Codex CLI",
        skill_name: "Codex",
        kind: CliKind::Codex,
        receipt: ReceiptRequirement::ExactHook,
    },
    Spec {
        id: "cursor",
        name: "Cursor Agent CLI",
        skill_name: "Cursor",
        kind: CliKind::Cursor,
        receipt: ReceiptRequirement::ExactHook,
    },
    Spec {
        id: "agy",
        name: "Antigravity CLI",
        skill_name: "Antigravity CLI",
        kind: CliKind::Agy,
        receipt: ReceiptRequirement::Screen,
    },
    Spec {
        id: "kimi",
        name: "Kimi Code CLI",
        skill_name: "Kimi",
        kind: CliKind::Kimi,
        receipt: ReceiptRequirement::ExactHook,
    },
    // The three below are wired from vendor documentation alone; no live
    // edge has been measured. Their manifests say so (version_tested =
    // "unverified") and declare an ack field, which is what the receipt
    // floor reads.
    Spec {
        id: "gemini",
        name: "Gemini CLI",
        skill_name: "Gemini",
        kind: CliKind::Gemini,
        receipt: ReceiptRequirement::ExactHook,
    },
    Spec {
        id: "qwen",
        name: "Qwen Code",
        skill_name: "Qwen",
        kind: CliKind::Qwen,
        receipt: ReceiptRequirement::ExactHook,
    },
    Spec {
        id: "goose",
        name: "goose",
        skill_name: "goose",
        kind: CliKind::Goose,
        receipt: ReceiptRequirement::ExactHook,
    },
];

/// The shared skill file every consumer that reads `~/.agents/skills`
/// receives once. Codex, Cursor, Gemini (documented alias of
/// `~/.gemini/skills`), and goose read it, and so do the skill-only
/// consumers in `crate::skillseed`. One copy, because a vendor that reads
/// two of its skill roots warns about the duplicate (MEASURED on Gemini CLI
/// 0.45.2: "Skill conflict detected").
pub(crate) fn shared_agents_skill(user_home: &Path) -> AssetLocation {
    AssetLocation {
        root: user_home.join(".agents"),
        relative: PathBuf::from("skills/cyclops/SKILL.md"),
    }
}

pub(crate) fn spec(kind: CliKind) -> &'static Spec {
    SHIPPED
        .iter()
        .find(|spec| spec.kind == kind)
        .expect("every CLI kind has one shipped consumer")
}

impl Spec {
    pub(crate) fn locations(self, user_home: &Path) -> Locations {
        let codex_root = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| user_home.join(".codex"));
        self.locations_with_codex_root(user_home, &codex_root)
    }

    fn locations_with_codex_root(self, user_home: &Path, codex_root: &Path) -> Locations {
        // Codex hooks stay at the user-level root. A project-local hooks.json
        // is ignored until that directory is trusted, so a non-interactive
        // launch can otherwise appear wired while receiving no events.
        let install_root = match self.kind {
            CliKind::Claude => user_home.join(".claude"),
            CliKind::Codex => codex_root.to_path_buf(),
            CliKind::Cursor => user_home.join(".cursor"),
            CliKind::Agy => user_home.join(".gemini/antigravity-cli"),
            CliKind::Kimi => std::env::var_os("KIMI_CODE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| user_home.join(".kimi-code")),
            // Antigravity CLI lives under ~/.gemini/antigravity-cli, so
            // ~/.gemini itself exists on every AGY machine and proves
            // nothing about Gemini CLI. MEASURED 2026-09-04: with AGY
            // installed and Gemini CLI never run, ~/.gemini/tmp was absent;
            // it appeared the moment Gemini CLI 0.45.2 started, and AGY keeps
            // its own installation_id and settings.json inside its subdirectory.
            CliKind::Gemini => gemini_home(user_home).join("tmp"),
            // Documented: QWEN_HOME "customizes the global configuration
            // directory (default: ~/.qwen)".
            CliKind::Qwen => std::env::var_os("QWEN_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| user_home.join(".qwen")),
            // Documented: ~/.config/goose/config.yaml on macOS and Linux.
            CliKind::Goose => user_home.join(".config/goose"),
        };
        let hook = match self.kind {
            CliKind::Claude => AssetLocation {
                root: install_root.clone(),
                relative: PathBuf::from("settings.json"),
            },
            CliKind::Codex | CliKind::Cursor => AssetLocation {
                root: install_root.clone(),
                relative: PathBuf::from("hooks.json"),
            },
            CliKind::Agy => AssetLocation {
                root: user_home.join(".agents"),
                relative: PathBuf::from("hooks.json"),
            },
            CliKind::Kimi => AssetLocation {
                root: install_root.clone(),
                relative: PathBuf::from("config.toml"),
            },
            CliKind::Gemini => AssetLocation {
                root: gemini_home(user_home),
                relative: PathBuf::from("settings.json"),
            },
            CliKind::Qwen => AssetLocation {
                root: install_root.clone(),
                relative: PathBuf::from("settings.json"),
            },
            // goose reads hooks from plugin directories, never from its
            // config.yaml; a hook-only plugin is one hooks/hooks.json under a
            // directory named for the plugin.
            CliKind::Goose => AssetLocation {
                root: user_home.join(".agents/plugins/cyclops"),
                relative: PathBuf::from("hooks/hooks.json"),
            },
        };
        let skill = match self.kind {
            CliKind::Claude | CliKind::Agy | CliKind::Kimi | CliKind::Qwen => AssetLocation {
                root: install_root.clone(),
                relative: PathBuf::from("skills/cyclops/SKILL.md"),
            },
            CliKind::Codex | CliKind::Cursor | CliKind::Gemini | CliKind::Goose => {
                shared_agents_skill(user_home)
            }
        };
        Locations {
            install_root,
            hook,
            skill,
        }
    }
}

/// Gemini CLI's own directory. MEASURED in the installed bundle:
/// `process.env.GEMINI_CLI_HOME || join(homedir, ".gemini")`.
fn gemini_home(user_home: &Path) -> PathBuf {
    std::env::var_os("GEMINI_CLI_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home.join(".gemini"))
}

/// Resolve the config root that proves a consumer is installed.
///
/// Codex alone supports an environment override. The override changes its
/// hook config root, not the shared Cyclops skill destination.
pub(crate) fn root(kind: CliKind, user_home: &Path) -> PathBuf {
    spec(kind).locations(user_home).install_root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_owns_each_shipped_consumers_setup_facts() {
        let home = Path::new("/users/operator");
        let codex = Path::new("/configured/codex");
        let facts: Vec<_> = SHIPPED
            .iter()
            .map(|spec| {
                let locations = spec.locations_with_codex_root(home, codex);
                (
                    spec.id,
                    spec.skill_name,
                    spec.receipt.tier(),
                    locations.install_root,
                    locations.hook.path(),
                    locations.skill.path(),
                )
            })
            .collect();

        assert_eq!(
            facts,
            vec![
                (
                    "claude",
                    "Claude Code",
                    1,
                    home.join(".claude"),
                    home.join(".claude/settings.json"),
                    home.join(".claude/skills/cyclops/SKILL.md"),
                ),
                (
                    "codex",
                    "Codex",
                    1,
                    codex.to_path_buf(),
                    codex.join("hooks.json"),
                    home.join(".agents/skills/cyclops/SKILL.md"),
                ),
                (
                    "cursor",
                    "Cursor",
                    1,
                    home.join(".cursor"),
                    home.join(".cursor/hooks.json"),
                    home.join(".agents/skills/cyclops/SKILL.md"),
                ),
                (
                    "agy",
                    "Antigravity CLI",
                    2,
                    home.join(".gemini/antigravity-cli"),
                    home.join(".agents/hooks.json"),
                    home.join(".gemini/antigravity-cli/skills/cyclops/SKILL.md"),
                ),
                (
                    "kimi",
                    "Kimi",
                    1,
                    home.join(".kimi-code"),
                    home.join(".kimi-code/config.toml"),
                    home.join(".kimi-code/skills/cyclops/SKILL.md"),
                ),
                (
                    "gemini",
                    "Gemini",
                    1,
                    home.join(".gemini/tmp"),
                    home.join(".gemini/settings.json"),
                    home.join(".agents/skills/cyclops/SKILL.md"),
                ),
                (
                    "qwen",
                    "Qwen",
                    1,
                    home.join(".qwen"),
                    home.join(".qwen/settings.json"),
                    home.join(".qwen/skills/cyclops/SKILL.md"),
                ),
                (
                    "goose",
                    "goose",
                    1,
                    home.join(".config/goose"),
                    home.join(".agents/plugins/cyclops/hooks/hooks.json"),
                    home.join(".agents/skills/cyclops/SKILL.md"),
                ),
            ]
        );
    }

    #[test]
    fn exact_hook_receipts_fail_closed_without_declared_ack_capability() {
        assert!(!ReceiptRequirement::ExactHook.accepts(None));
        assert!(!ReceiptRequirement::ExactHook.accepts(Some(false)));
        assert!(ReceiptRequirement::ExactHook.accepts(Some(true)));
        assert!(ReceiptRequirement::Screen.accepts(None));
    }
}
