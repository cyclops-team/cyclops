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
//! `CYCLOPS_NO_VENDOR_HOOKS`, and never replacing bytes this project did
//! not write ([`EVER_SHIPPED_FNV64`], the same edit-detection rule as
//! `crate::manifests`).

use std::path::{Path, PathBuf};

/// The skill body the binary carries.
pub(crate) const SHIPPED: &str = include_str!("../../../skills/cyclops/SKILL.md");

/// Where the skill goes under the agent's dot-directory.
pub fn skill_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("skills").join("cyclops").join("SKILL.md")
}

/// FNV-1a 64 of every skill body this project has released, the current
/// one included. Same contract as `crate::manifests::EVER_SHIPPED_FNV64`:
/// a file on disk whose hash is in this list is a seed nobody edited, so
/// a newer shipped body may replace it; any other content is the
/// operator's and is never touched. The test below fails until the
/// current body is listed, and prints the hash to append.
///
/// Released, not every body that ever compiled. A hash here is permission
/// to overwrite a file on somebody's disk, and an unreleased intermediate
/// was never seeded onto one, so listing it would only claim that
/// authority over bytes that could just as well be an operator's own.
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
    /// The shipped body is now at [`SeededSkill::path`]: a fresh install
    /// or an unedited old seed upgraded.
    Written,
    /// A file was already there and was left alone: either it is already
    /// current, or the operator edited it and their copy outranks ours.
    Kept,
    /// No consumer for this destination is installed. Nothing was created.
    NoAgent,
    /// The write failed; the sentence says where and why.
    Problem(String),
}

/// One seed attempt against one agent directory.
pub struct SeededSkill {
    pub consumer: &'static str,
    pub path: PathBuf,
    pub outcome: Outcome,
}

/// Put the shipped skill under `agent_dir`, keeping any operator edit.
fn seed_into(consumer: &'static str, installed: bool, agent_dir: &Path) -> SeededSkill {
    let path = skill_path(agent_dir);
    if !installed {
        return SeededSkill {
            consumer,
            path,
            outcome: Outcome::NoAgent,
        };
    }
    if let Ok(existing) = std::fs::read(&path) {
        // Already current needs no write, and anything the operator
        // typed outranks the shipped copy.
        if existing == SHIPPED.as_bytes() || !unedited_seed(&existing) {
            return SeededSkill {
                consumer,
                path,
                outcome: Outcome::Kept,
            };
        }
    } else if path.exists() {
        // There but unreadable: not ours to replace.
        return SeededSkill {
            consumer,
            path,
            outcome: Outcome::Kept,
        };
    }
    let dir = path.parent().expect("skill path always has a parent");
    let outcome = match std::fs::create_dir_all(dir)
        .map_err(|e| format!("create {}: {e}", dir.display()))
        .and_then(|()| {
            std::fs::write(&path, SHIPPED).map_err(|e| format!("write {}: {e}", path.display()))
        }) {
        Ok(()) => Outcome::Written,
        Err(cause) => Outcome::Problem(cause),
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
    let claude = crate::consumer::root(crate::hookset::CliKind::Claude, &home);
    let codex_installed = crate::consumer::root(crate::hookset::CliKind::Codex, &home).is_dir();
    let cursor_installed = crate::consumer::root(crate::hookset::CliKind::Cursor, &home).is_dir();
    let (shared_consumer, shared_installed) = match (codex_installed, cursor_installed) {
        (true, true) => ("Codex and Cursor", true),
        (true, false) => ("Codex", true),
        (false, true) => ("Cursor", true),
        (false, false) => ("Codex and Cursor", false),
    };
    let shared = home.join(".agents");
    let agy = crate::consumer::root(crate::hookset::CliKind::Agy, &home);
    vec![
        seed_into("Claude Code", claude.is_dir(), &claude),
        seed_into(shared_consumer, shared_installed, &shared),
        seed_into("Antigravity CLI", agy.is_dir(), &agy),
    ]
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

    /// Same contract as the manifest list: whoever changes the skill sees
    /// this fail and appends the new hash, so the replaced version stays
    /// recognizable as an unedited seed forever after.
    #[test]
    fn the_ever_shipped_list_contains_the_current_body() {
        assert!(
            EVER_SHIPPED_FNV64.contains(&fnv64(SHIPPED.as_bytes()).as_str()),
            "the current skill body is not in EVER_SHIPPED_FNV64; add {}",
            fnv64(SHIPPED.as_bytes())
        );
    }

    /// The whole policy in one run: no vendor dir means nothing is
    /// created, a fresh seed writes, a rerun keeps quiet, an edit
    /// survives, and shipped-but-stale bytes upgrade.
    #[test]
    fn the_seed_respects_the_vendor_dir_and_the_operators_edits() {
        let root = cyclops_proto::scratch::scratch_dir("cyc-skillseed");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        let agent_dir = root.join(".claude");

        // Not installed: nothing appears, not even the directory.
        let absent = seed_into("test agent", agent_dir.is_dir(), &agent_dir);
        assert_eq!(absent.outcome, Outcome::NoAgent);
        assert!(!agent_dir.exists(), "seeding invented the vendor dir");
        assert_eq!(note(&absent), None);

        // Installed: the skill lands and says so.
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        let first = seed_into("test agent", agent_dir.is_dir(), &agent_dir);
        assert_eq!(first.outcome, Outcome::Written);
        assert_eq!(
            std::fs::read_to_string(&first.path).expect("written"),
            SHIPPED
        );
        assert!(note(&first)
            .expect("a write speaks")
            .contains("installed the test agent skill"));

        // Rerun: current file, no write, no note.
        let second = seed_into("test agent", agent_dir.is_dir(), &agent_dir);
        assert_eq!(second.outcome, Outcome::Kept);
        assert_eq!(note(&second), None);

        // An operator edit outranks the shipped copy on every later run.
        std::fs::write(&first.path, "# my own notes\n").expect("edit");
        let third = seed_into("test agent", agent_dir.is_dir(), &agent_dir);
        assert_eq!(third.outcome, Outcome::Kept);
        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            "# my own notes\n"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
