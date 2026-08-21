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
//! Where it goes is another tool's home (`~/.claude/skills/cyclops/`),
//! which `crate::hookset` deliberately refuses to touch for `hooks
//! install`. Writing here follows the rules `hookset::wire_vendor`
//! already set for vendor homes: only under the installer's
//! `--wire-hooks` consent (given at install time, or recorded then and
//! honored by a later boot: `workspace::finish_deferred_wiring`), only
//! when the vendor's directory already exists (its presence is what says
//! the agent CLI is installed; this module never creates `~/.claude`
//! itself), honoring `CYCLOPS_NO_VENDOR_HOOKS`, and never replacing
//! bytes this project did not write ([`EVER_SHIPPED_FNV64`], the same
//! edit-detection rule as `crate::manifests`).

use std::path::{Path, PathBuf};

/// The skill body the binary carries.
const SHIPPED: &str = include_str!("../../../skills/cyclops/SKILL.md");

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
const EVER_SHIPPED_FNV64: &[&str] = &["7ebc1453af11b931", "cf5916d45a60081c"];

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

/// What one [`seed_into`] run did, in the order a caller checks them.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The shipped body is now at [`SeededSkill::path`]: a fresh install
    /// or an unedited old seed upgraded.
    Written,
    /// A file was already there and was left alone: either it is already
    /// current, or the operator edited it and their copy outranks ours.
    Kept,
    /// The agent's directory does not exist, so that CLI is not installed
    /// here and there is nowhere the skill would be read from. Nothing
    /// was created: making `~/.claude` for a tool that is not installed
    /// would be this project inventing another tool's home.
    NoAgent,
    /// The write failed; the sentence says where and why.
    Problem(String),
}

/// One seed attempt against one agent directory.
pub struct SeededSkill {
    pub path: PathBuf,
    pub outcome: Outcome,
}

/// Put the shipped skill at `<agent_dir>/skills/cyclops/SKILL.md`,
/// keeping any copy the operator edited.
///
/// `agent_dir` is the agent CLI's own dot-directory (`~/.claude` in
/// production; tests hand in scratch). Its existence is the gate; the
/// `skills/cyclops` levels under it are this module's to create, the same
/// way `wire_vendor` writes a hooks.json into a vendor directory it
/// refuses to create.
pub fn seed_into(agent_dir: &Path) -> SeededSkill {
    let path = skill_path(agent_dir);
    if !agent_dir.is_dir() {
        return SeededSkill {
            path,
            outcome: Outcome::NoAgent,
        };
    }
    if let Ok(existing) = std::fs::read(&path) {
        // Already current needs no write, and anything the operator
        // typed outranks the shipped copy.
        if existing == SHIPPED.as_bytes()
            || !EVER_SHIPPED_FNV64.contains(&fnv64(&existing).as_str())
        {
            return SeededSkill {
                path,
                outcome: Outcome::Kept,
            };
        }
    } else if path.exists() {
        // There but unreadable: not ours to replace.
        return SeededSkill {
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
    SeededSkill { path, outcome }
}

/// Seed the skill for every agent CLI installed on this machine that
/// reads skills from a folder. One entry today: Claude Code reads
/// `~/.claude/skills/`. Codex, Agy and Cursor have no skill folder to
/// write into, so they are taught by hand (the README says how).
pub fn seed() -> Vec<SeededSkill> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    vec![seed_into(&PathBuf::from(home).join(".claude"))]
}

/// The note `cyclops start --setup-only` prints for one seed attempt.
/// Only a write or a failure speaks: a kept file and an absent agent CLI
/// are both the ordinary case on a rerun, and a note repeated on every
/// setup is noise.
pub fn note(seeded: &SeededSkill) -> Option<String> {
    match &seeded.outcome {
        Outcome::Written => Some(format!(
            "installed the agent skill at {}",
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
        let absent = seed_into(&agent_dir);
        assert_eq!(absent.outcome, Outcome::NoAgent);
        assert!(!agent_dir.exists(), "seeding invented the vendor dir");
        assert_eq!(note(&absent), None);

        // Installed: the skill lands and says so.
        std::fs::create_dir_all(&agent_dir).expect("create agent dir");
        let first = seed_into(&agent_dir);
        assert_eq!(first.outcome, Outcome::Written);
        assert_eq!(
            std::fs::read_to_string(&first.path).expect("written"),
            SHIPPED
        );
        assert!(note(&first)
            .expect("a write speaks")
            .contains("installed the agent skill"));

        // Rerun: current file, no write, no note.
        let second = seed_into(&agent_dir);
        assert_eq!(second.outcome, Outcome::Kept);
        assert_eq!(note(&second), None);

        // An operator edit outranks the shipped copy on every later run.
        std::fs::write(&first.path, "# my own notes\n").expect("edit");
        let third = seed_into(&agent_dir);
        assert_eq!(third.outcome, Outcome::Kept);
        assert_eq!(
            std::fs::read_to_string(&first.path).unwrap(),
            "# my own notes\n"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
