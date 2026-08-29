//! The detection manifests that ship with the binary, and getting them
//! into the cyclops home.
//!
//! A manifest is everything cyclops knows about one agent CLI: which
//! processes it runs as, how to read busy from idle off the pane, how to
//! type into it (docs/reference/MANIFESTS.md). Without one, a pane reads `? unknown`
//! and a delivery to it ends in attention_required. So a machine with no
//! manifests has a cyclops that starts, watches, reports, and cannot
//! deliver a single message.
//!
//! They live in the repo at `resources/manifests/` and are compiled in here, the same
//! way `workspace.rs` compiles in the layout presets and for the same
//! reason: an install is two binaries, and it has to work before it has
//! files. `cyclops start` writes them into `$CYCLOPS_HOME/manifests`, which
//! is where cyclopsd looks when `manifest_dir` is unset.
//!
//! Writing never overwrites an edit. These files are meant to be edited, a
//! rule that has already been measured against a real CLI is worth more than
//! the shipped guess, and a seed that clobbered would throw that away on
//! every run. What the seed does replace is its own writing: a file whose
//! bytes are a version this project shipped is a seed nobody touched, and a
//! newer shipped body takes its place ([`EVER_SHIPPED_FNV64`]). Same rule
//! and same reason as [`crate::themeseed`].

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use cyclops_state::{CreateFileOutcome, StateRoot};

use crate::hash::fnv64;

/// Every manifest the binary carries, by file name.
const SHIPPED: &[(&str, &str)] = &[
    (
        "agy.toml",
        include_str!("../../../resources/manifests/agy.toml"),
    ),
    (
        "claude.toml",
        include_str!("../../../resources/manifests/claude.toml"),
    ),
    (
        "codex.toml",
        include_str!("../../../resources/manifests/codex.toml"),
    ),
    (
        "cursor.toml",
        include_str!("../../../resources/manifests/cursor.toml"),
    ),
];

/// Where manifests go, and where cyclopsd looks with no `manifest_dir` set.
pub fn dir(home: &Path) -> PathBuf {
    home.join("manifests")
}

/// FNV-1a 64 of every manifest body this project has ever shipped, the
/// current ones included. A file on disk whose hash is in this list is a
/// seed nobody edited, so a newer shipped body may replace it; any other
/// content is a measurement somebody took against a real CLI and is never
/// touched.
///
/// Measured cost of not having it: all ten manifest bodies in this repo's
/// history predate the `launch` key, so every home seeded before that key
/// existed holds a claude.toml and a codex.toml that say nothing about how
/// to start the CLI. On those homes `cyclops start --preset duo --agents
/// claude,codex` exits 2 saying the manifest has no launch command, and
/// reinstalling does not help, because the old seed skipped every name it
/// already found. That is the maintainer's own machines, and every install
/// that predates the key.
///
/// Regenerate when a shipped manifest changes: hash the historical blobs
/// of resources/manifests/ (and the old manifests/ path) on released
/// refs, plus the current bodies. The test below fails until the current
/// bodies are listed, and prints the hash to append.
///
/// Two things stay out. Bodies from an unreleased branch: nobody was ever
/// seeded with those, so listing one only claims replacement authority
/// over bytes that could just as well be an operator's own. And hashes
/// from any other seeded file: this list answers "did the operator edit
/// this manifest", and a skill body's hash landing here would make an
/// edited manifest look untouched.
const EVER_SHIPPED_FNV64: &[&str] = &[
    "000da241c916d3cb",
    "0a90724a6ccb9c0a",
    "0e7c902fb67d702a",
    "12765e334728f40b",
    "21a94a206aefa357",
    "2893de300e4fc944",
    "2949a1d316e76d5b",
    "2a8c5b29dff89435",
    "2d760e8dbff7a191",
    "316b4c51088fc315",
    "bf66db73f5d56a8f",
    "b4ef7ddba83f94a1",
    "3292eae226f54f79",
    "39e895dafb8bedb7",
    "3e966f9f7049d206",
    "3ec9936975418c27",
    "3ee05959c01113a7",
    "3e1f2f2587609500",
    "3f2860fa1275d798",
    "447ac3d6d4fb9127",
    "44c3c94fe03c18c9",
    "4af4029b9d2910e3",
    "57c2bf8d8b894ce0",
    "5fb9fab4521686ad",
    "645013f86f1bcb41",
    "6680bd20b0f93e56",
    "69c853cdcc84e970",
    "6a4f11941f4c78be",
    "6f824b54f50c63b5",
    "75f3c5daed1304f1",
    "7a9754b750109c5e",
    "81e7e3ad63748a7f",
    "821ec0a1af0f8cc1",
    "84b0dfa8ce699bb0",
    "84c43e4255939728",
    "85f276e9afffc42d",
    "884a6e869dbdea99",
    "92e40157e2ad9b86",
    "970aabc764b3acd4",
    "97778c580f34b125",
    "98ae3fa4eca78cf3",
    "9947c2cf75719a69",
    "9a67be48357ff9e0",
    "9be123836a9dac67",
    "9eee760afa8f5ead",
    "bc4bc371b40e71ca",
    "bf50a35c755a593c",
    "c60d4aff7fe2a5d2",
    "c86ba6c35e662ee6",
    "cd2508367907b954",
    "cdc9378b2adb994e",
    "dc0b06781eb5a812",
    "dcf8e732f46b397c",
    "e2a3e259f46c4dfc",
    "192c1584efd10f6f",
    "ee41a0a36c59c466",
    "f6c7c7aaa830babb",
    "9e4d60c2da88e88e",
    "ed6a0d16eff01e23",
];

/// True when these bytes are a manifest this project wrote, so replacing
/// them loses nothing the operator put there.
pub(crate) fn unedited_seed(body: &[u8]) -> bool {
    EVER_SHIPPED_FNV64.contains(&fnv64(body).as_str())
}

pub(crate) fn shipped_body(id: &str) -> Option<&'static str> {
    let name = format!("{id}.toml");
    SHIPPED
        .iter()
        .find(|(shipped_name, _)| *shipped_name == name)
        .map(|(_, body)| *body)
}

/// What one [`seed`] run did.
pub struct Seeded {
    pub dir: PathBuf,
    /// Shipped files written this run, fresh seeds and upgrades alike.
    pub written: Vec<String>,
    /// Shipped files already there, left exactly as they were.
    pub kept: Vec<String>,
    /// Why a file could not be written, one sentence each.
    pub problems: Vec<String>,
}

impl Seeded {
    /// True when the directory holds none of the shipped manifests and
    /// nothing could be written, which is the broken install: cyclopsd will
    /// bind no pane and deliver nothing.
    pub fn none_installed(&self) -> bool {
        self.written.is_empty() && self.kept.is_empty()
    }
}

/// Put the shipped manifests in `<home>/manifests`, keeping every file that
/// is already there.
///
/// Runs on every `cyclops start`, not only the first: an upgrade that adds
/// a manifest lands on the next start, and a home that predates this seed
/// gets one without a reinstall. A file already present is rewritten only
/// when its bytes are a version this project shipped
/// ([`EVER_SHIPPED_FNV64`]): that file is a seed nobody touched, and
/// leaving it stale is how a pre-`launch` claude.toml kept `--agents` dead
/// on every install that predates the key. Anything else on disk is a
/// measurement against a real CLI and survives every run.
pub fn seed(home: &Path) -> Seeded {
    let dir = dir(home);
    let mut out = Seeded {
        dir: dir.clone(),
        written: Vec::new(),
        kept: Vec::new(),
        problems: Vec::new(),
    };
    let root = match StateRoot::open_or_create(home) {
        Ok(root) => root,
        Err(e) => {
            out.problems.push(format!("create {}: {e}", dir.display()));
            return out;
        }
    };
    for (name, body) in SHIPPED {
        let path = dir.join(name);
        let descendant = Path::new("manifests").join(name);
        let existing = match root.open_read(&descendant) {
            Ok(Some(mut file)) => {
                let mut bytes = Vec::new();
                if let Err(e) = file.read_to_end(&mut bytes) {
                    out.problems.push(format!("read {}: {e}", path.display()));
                    continue;
                }
                Some(bytes)
            }
            Ok(None) => None,
            Err(e) => {
                out.problems.push(format!("read {}: {e}", path.display()));
                continue;
            }
        };
        if let Some(existing) = existing {
            if existing == body.as_bytes() || !unedited_seed(&existing) {
                out.kept.push((*name).to_string());
                continue;
            }
            match root.replace_file(&descendant, body.as_bytes()) {
                Ok(()) => out.written.push((*name).to_string()),
                Err(e) => out.problems.push(format!("write {}: {e}", path.display())),
            }
            continue;
        }
        match root.create_file_once(&descendant, body.as_bytes()) {
            Ok(CreateFileOutcome::Created) => out.written.push((*name).to_string()),
            Ok(CreateFileOutcome::AlreadyExists) => match root.open_read(&descendant) {
                Ok(Some(_)) => out.kept.push((*name).to_string()),
                Ok(None) => out.problems.push(format!(
                    "write {}: file disappeared after concurrent creation",
                    path.display()
                )),
                Err(e) => out.problems.push(format!("read {}: {e}", path.display())),
            },
            Err(e) => out.problems.push(format!("write {}: {e}", path.display())),
        }
    }
    out
}

/// One agent CLI cyclops knows about, from the manifest that describes it.
///
/// The launch command is what `cyclops start --agents <id>` runs. None
/// means the manifest detects that CLI without saying how to start it,
/// which is a refusal and not a guess: the wrong binary name in a pane is
/// a shell error the operator has to read out of a pane to understand.
pub struct Known {
    pub launch: Option<String>,
    /// The file that said so, for a line telling the reader where to add
    /// the key.
    pub path: PathBuf,
    /// `[hooks].settings_flag`, when the CLI takes its hook config as a
    /// launch argument. Carried here so `--agents` can append one without
    /// reparsing the manifest it already read.
    pub settings_flag: Option<String>,
}

/// Every agent CLI this home would use, by manifest id.
///
/// Two sources, and the order between them is the point. A file in the
/// home wins, because that is the copy cyclopsd reads and the copy the
/// operator edits. The shipped set fills in the ids the home does not have
/// yet, because [`seed`] writes exactly those files later in the same
/// `cyclops start` and refusing an id that is about to exist would be a
/// lie about a first run.
///
/// One exception, and it is the rule [`seed`] runs on: a home file whose
/// bytes are a version this project shipped is this project's own writing,
/// not the operator's, so the shipped copy of that id wins here too. Both
/// halves are needed. `cyclops start` resolves `--agents` before it seeds,
/// so on a home carrying a pre-`launch` claude.toml the seed's upgrade
/// lands one step too late to answer this run, and the run refuses. A home
/// file whose id the shipped set does not carry is kept either way: that
/// file is still what cyclopsd reads.
///
/// A file that does not parse is left out rather than reported. The daemon
/// is what reads manifests for real and what says a directory is broken;
/// this is one key lookup, and it does not get to speak for the install.
pub fn known(home: &Path) -> BTreeMap<String, Known> {
    let dir = dir(home);
    let mut out = BTreeMap::new();
    for (name, body) in SHIPPED {
        // Named at the path the seed will write it to, so a line about a
        // missing key points at the file the reader will have.
        if let Some((id, k)) = parsed(body, dir.join(name)) {
            out.insert(id, k);
        }
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some((id, k)) = parsed(&text, path) else {
            continue;
        };
        // The id check matters only once an id leaves SHIPPED: an unedited
        // seed with no shipped counterpart is still the file cyclopsd
        // reads, and dropping it would refuse an id that works.
        if unedited_seed(text.as_bytes()) && out.contains_key(&id) {
            continue;
        }
        out.insert(id, k);
    }
    out
}

/// The id and launch command a manifest body carries, or None when it does
/// not parse.
fn parsed(text: &str, path: PathBuf) -> Option<(String, Known)> {
    let m = cyclops_manifest::Manifest::parse(text, &path).ok()?;
    Some((
        m.agent.id,
        Known {
            launch: m.agent.launch,
            path,
            settings_flag: m.hooks.settings_flag,
        },
    ))
}

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

/// The line `cyclops start` prints when it installed manifests.
///
/// Only when it actually wrote something. A run that found all of them
/// already there says nothing: a note repeated on every start is noise, and
/// the reader is looking for what changed.
pub fn installed(seeded: &Seeded) -> String {
    let n = seeded.written.len();
    let thing = if n == 1 { "manifest" } else { "manifests" };
    format!("wrote {n} detection {thing} to {}", seeded.dir.display())
}

/// The install is broken and nothing that follows can work.
///
/// Said in `cyclops start`'s own output, because every other line it prints
/// reports success and this is the one that says the success is empty.
pub fn nothing_installed(seeded: &Seeded) -> String {
    let why = match seeded.problems.first() {
        Some(p) => format!(": {p}"),
        None => String::new(),
    };
    format!(
        "no detection manifests in {}{why}. Cyclops can't tell what is running in a pane without them, so every pane reads unknown and no message can be delivered. Check you can write that directory, then run cyclops start again.",
        seeded.dir.display()
    )
}

/// Some manifests landed and some did not. Cyclops works for the CLIs that
/// got through, so this is a note beside a real ready line and not the
/// refusal above.
pub fn partly_installed(seeded: &Seeded) -> String {
    let cli = if seeded.problems.len() == 1 {
        "That agent CLI"
    } else {
        "Those agent CLIs"
    };
    format!(
        "{}. {cli} won't be detected; the manifests that landed are in {}.",
        seeded.problems.join("; "),
        seeded.dir.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt as _};

    /// The body the binary carries under `name`.
    fn shipped(name: &str) -> &'static str {
        SHIPPED
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, b)| *b)
            .expect("a shipped manifest by that name")
    }

    /// A file body out of this repo's history, by blob sha. None when git
    /// or the object is not reachable, which is a skip and not a failure:
    /// the object is evidence about what shipped, not something this build
    /// produces.
    fn git_blob(sha: &str) -> Option<String> {
        let out = std::process::Command::new("git")
            .args(["-C", env!("CARGO_MANIFEST_DIR"), "cat-file", "blob", sha])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    }

    /// The compiled-in copies have to be the files in the repo, and they
    /// have to be manifests cyclops can actually load. A binary shipping a
    /// manifest that does not parse would seed a home where every pane
    /// reads unknown, which is the failure this whole module exists to
    /// prevent.
    #[test]
    fn the_shipped_set_is_the_repo_and_it_parses() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
        let on_disk: Vec<String> = std::fs::read_dir(&repo)
            .expect("the repo manifests directory")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".toml"))
            .collect();
        assert_eq!(
            on_disk.len(),
            SHIPPED.len(),
            "manifests/ holds {on_disk:?}, the binary carries {:?}",
            SHIPPED.iter().map(|(n, _)| *n).collect::<Vec<_>>()
        );
        for (name, body) in SHIPPED {
            let text = std::fs::read_to_string(repo.join(name)).expect("shipped file exists");
            assert_eq!(&text, body, "{name} in the binary is not {name} on disk");
            cyclops_manifest::Manifest::parse(body, Path::new(name))
                .unwrap_or_else(|e| panic!("shipped {name} does not parse: {e}"));
        }
    }

    /// The rule the whole module turns on: a seed installs what is missing
    /// and touches nothing else. An edited manifest is a measurement
    /// somebody took against a real CLI, and the shipped file is a guess
    /// next to it.
    #[test]
    fn a_second_seed_keeps_the_edits_the_first_one_left_behind() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-seed-keep");
        let _ = std::fs::remove_dir_all(&home);

        let first = seed(&home);
        assert_eq!(first.written.len(), SHIPPED.len(), "{:?}", first.problems);
        assert!(first.kept.is_empty());
        assert!(!first.none_installed());
        assert_eq!(
            std::fs::metadata(dir(&home)).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(dir(&home).join("claude.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        // An edit, and a manifest of the reader's own.
        let edited = dir(&home).join("claude.toml");
        let mine = dir(&home).join("mycli.toml");
        std::fs::write(&edited, "# measured on 2.1.220\n").expect("edit");
        std::fs::write(&mine, "# mine\n").expect("write mine");

        let second = seed(&home);
        assert!(second.written.is_empty(), "{:?}", second.written);
        assert_eq!(second.kept.len(), SHIPPED.len());
        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "# measured on 2.1.220\n",
            "a later start overwrote an edited manifest"
        );
        assert!(mine.exists(), "a later start removed a manifest of my own");

        // A shipped manifest deleted by hand comes back, which is how a set
        // that gains a file reaches a home that already exists.
        std::fs::remove_file(dir(&home).join("agy.toml")).expect("remove");
        let third = seed(&home);
        assert_eq!(third.written, vec!["agy.toml"]);
        assert_eq!(
            installed(&third),
            format!("wrote 1 detection manifest to {}", dir(&home).display())
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// Every current shipped body must be in the ever-shipped hash list,
    /// which is what keeps the list honest: whoever changes a manifest sees
    /// this fail and appends the new hash, so the version they replaced
    /// stays recognizable as an unedited seed forever after.
    #[test]
    fn the_ever_shipped_list_contains_the_current_bodies() {
        for (name, body) in SHIPPED {
            assert!(
                unedited_seed(body.as_bytes()),
                "{name}'s current body is not in EVER_SHIPPED_FNV64; add {}",
                fnv64(body.as_bytes())
            );
        }
    }

    /// The stale-seed trap, closed. Until this rule a manifest already on
    /// disk was never read, so the `launch` key added to every shipped
    /// manifest reached no home that had been seeded even once, and
    /// `--agents` exited 2 on every install that predates the key.
    ///
    /// The real pre-`launch` bodies are blob-exact in git history and their
    /// hashes are in `EVER_SHIPPED_FNV64`, but they run 5-8 KB each and do
    /// not belong pasted into a test. What stands in for one here is a body
    /// that hashes into the list and is not what belongs under that name:
    /// the current codex.toml under claude.toml is exactly that, shipped
    /// bytes nobody typed. The negative goes first, because a rule that
    /// replaces too much is worse than the bug it fixes.
    #[test]
    fn an_unedited_old_seed_upgrades_and_an_edited_one_never_does() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-seed-upgrade");
        let _ = std::fs::remove_dir_all(&home);
        seed(&home);
        let claude = dir(&home).join("claude.toml");

        // Content this project never wrote is the operator's, and stays.
        std::fs::write(&claude, "# measured on 2.1.220\n").expect("edit");
        let kept = seed(&home);
        assert!(kept.written.is_empty(), "{:?}", kept.written);
        assert_eq!(kept.kept.len(), SHIPPED.len());
        assert_eq!(
            std::fs::read_to_string(&claude).unwrap(),
            "# measured on 2.1.220\n"
        );

        // Shipped bytes under the wrong name are a seed, not a decision.
        std::fs::write(&claude, shipped("codex.toml")).expect("plant");
        let upgraded = seed(&home);
        assert_eq!(upgraded.written, vec!["claude.toml".to_string()]);
        let now = std::fs::read_to_string(&claude).unwrap();
        assert_eq!(now, shipped("claude.toml"));
        assert!(
            now.contains("launch = \"claude\""),
            "the upgrade is what carries the launch key onto an old home"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The other half, and the one `--agents` actually hits first.
    /// `cyclops start` resolves the agent list before it seeds, so an
    /// upgrade written by [`seed`] lands one step too late for the run that
    /// needed it. A home file that is an unedited seed must therefore lose
    /// to the shipped copy of its id here as well, or the first `cyclops
    /// start --agents` on a pre-`launch` home still exits 2.
    ///
    /// One file carries the id, so the answer cannot depend on `read_dir`
    /// order.
    #[test]
    fn an_unedited_seed_does_not_outrank_the_shipped_copy_of_its_id() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-known-stale");
        let _ = std::fs::remove_dir_all(&home);
        seed(&home);

        // Shipped bytes under a name nobody ships them under: still this
        // project's writing, so the shipped claude.toml keeps answering.
        std::fs::remove_file(dir(&home).join("claude.toml")).expect("remove");
        std::fs::write(dir(&home).join("stale.toml"), shipped("claude.toml")).expect("plant");
        let after = known(&home);
        assert!(
            after["claude"].path.ends_with("manifests/claude.toml"),
            "an unedited seed answered for claude: {:?}",
            after["claude"].path
        );
        assert_eq!(after["claude"].launch.as_deref(), Some("claude"));

        // An edited file still wins, which is the rule this must not break.
        std::fs::write(
            dir(&home).join("stale.toml"),
            "[agent]\nid = \"claude\"\ndisplay_name = \"C\"\nlaunch = \"claude --resume\"\n",
        )
        .expect("edit");
        assert_eq!(
            known(&home)["claude"].launch.as_deref(),
            Some("claude --resume")
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The defect itself, against the bytes that caused it.
    ///
    /// These are resources/manifests/claude.toml and codex.toml as they
    /// stood before the `launch` key existed, read out of git by blob sha
    /// so nothing here moves when a branch does. That is what every home
    /// seeded before the key still holds, and on those homes `cyclops start
    /// --agents claude,codex` exited 2. Both halves are checked in the
    /// order `cyclops start` runs them: [`known`] answers first, [`seed`]
    /// writes after.
    ///
    /// Skipped when the object is not reachable, a checkout with no git or
    /// a rewritten history. The rule is covered by the two tests above
    /// either way; what this adds is that it fires on the real bodies.
    #[test]
    fn the_pre_launch_manifests_every_old_home_holds_get_their_launch_key() {
        // (blob sha, manifest id, the launch command it is missing)
        const PRE_LAUNCH: &[(&str, &str, &str)] = &[
            (
                "ee4f9d1618e194ea0cd8958db0bc009721aaddb1",
                "claude",
                "claude",
            ),
            ("da17f332838145403b323167a6d79c76aaa95144", "codex", "codex"),
        ];

        let home = cyclops_proto::scratch::scratch_dir("cyc-prelaunch");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(dir(&home)).expect("create manifests dir");

        // Plant what git can still hand over, and check on the way that
        // each body really is the pre-launch shape this is arguing about.
        let mut planted: Vec<(&str, &str)> = Vec::new();
        for (blob, id, launch) in PRE_LAUNCH {
            let Some(old) = git_blob(blob) else { continue };
            let file = dir(&home).join(format!("{id}.toml"));
            let table: toml::Table = old.parse().expect("the old body is valid TOML");
            let agent = table
                .get("agent")
                .and_then(toml::Value::as_table)
                .expect("the old body has an agent table");
            assert_eq!(agent.get("id").and_then(toml::Value::as_str), Some(*id));
            assert_eq!(
                agent.get("launch"),
                None,
                "{id}: this blob is not pre-launch"
            );
            assert!(
                unedited_seed(old.as_bytes()),
                "the pre-launch {id} body is not in EVER_SHIPPED_FNV64; add {}",
                fnv64(old.as_bytes())
            );
            std::fs::write(&file, &old).expect("plant the old body");
            planted.push((id, launch));
        }
        if planted.is_empty() {
            return;
        }

        // 1. The lookup --agents runs on, before anything is seeded.
        let before = known(&home);
        for (id, launch) in &planted {
            assert_eq!(
                before[*id].launch.as_deref(),
                Some(*launch),
                "--agents {id} would still exit 2 on a pre-launch home"
            );
        }

        // 2. And the seed puts the current body on disk, so the next run
        //    reads the launch key from the file the operator can see.
        let seeded = seed(&home);
        for (id, launch) in &planted {
            let name = format!("{id}.toml");
            assert!(seeded.written.contains(&name), "{id} was not upgraded");
            let now = std::fs::read_to_string(dir(&home).join(&name)).unwrap();
            assert_eq!(now, shipped(&name));
            assert!(now.contains(&format!("launch = \"{launch}\"")));
        }

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The lookup `--agents` runs on: the shipped set answers on a home
    /// that has not been seeded yet, an edited file in the home wins over
    /// the shipped copy of the same id, a manifest of the reader's own is
    /// there too, and a file that does not parse is simply not offered.
    #[test]
    fn an_edited_home_manifest_wins_over_the_shipped_copy_of_the_same_cli() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-known");
        let _ = std::fs::remove_dir_all(&home);

        // Nothing on disk: the shipped set is still the answer, because
        // this same run is about to write it.
        let fresh = known(&home);
        assert_eq!(fresh["claude"].launch.as_deref(), Some("claude"));
        assert_eq!(fresh["cursor"].launch.as_deref(), Some("cursor-agent"));
        assert!(fresh["claude"].path.ends_with("manifests/claude.toml"));

        seed(&home);
        let head = "[agent]\nid = \"claude\"\ndisplay_name = \"Claude Code\"\n";
        std::fs::write(
            dir(&home).join("claude.toml"),
            format!("{head}launch = \"claude --resume\"\n"),
        )
        .expect("edit claude");
        std::fs::write(
            dir(&home).join("mine.toml"),
            "[agent]\nid = \"mine\"\ndisplay_name = \"Mine\"\n",
        )
        .expect("write mine");
        std::fs::write(dir(&home).join("broken.toml"), "id = ").expect("write broken");

        let after = known(&home);
        assert_eq!(after["claude"].launch.as_deref(), Some("claude --resume"));
        assert_eq!(after["mine"].launch, None, "a manifest need not launch");
        assert!(!after.contains_key("broken"));
        assert_eq!(after["codex"].launch.as_deref(), Some("codex"));

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The middle case, which the seed reaches when one file fails and
    /// others do not. Cyclops still works for the CLIs that landed, so it
    /// is a note beside a real ready line, and it has to name what was
    /// lost rather than claim the install is dead.
    #[test]
    fn a_partial_install_names_what_did_not_land() {
        let dir = PathBuf::from("/h/manifests");
        let one = Seeded {
            dir: dir.clone(),
            written: vec!["agy.toml".into()],
            kept: Vec::new(),
            problems: vec!["write /h/manifests/claude.toml: Permission denied".into()],
        };
        assert!(!one.none_installed());
        assert_eq!(
            partly_installed(&one),
            "write /h/manifests/claude.toml: Permission denied. That agent CLI won't be detected; the manifests that landed are in /h/manifests."
        );
        let two = Seeded {
            problems: vec!["write a: no".into(), "write b: no".into()],
            ..one
        };
        assert!(partly_installed(&two).contains("Those agent CLIs won't be detected"));
    }

    /// A directory that cannot be written is the broken install, and the
    /// line says which directory, why, and what to do.
    #[test]
    fn a_seed_that_writes_nothing_says_the_install_cannot_work() {
        let root = cyclops_proto::scratch::scratch_dir("cyc-seed-blocked");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        // A file where the manifests directory has to go: create_dir_all
        // fails, and nothing can be written under it.
        let home = root.join("home");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::write(dir(&home), "not a directory").expect("occupy the path");

        let seeded = seed(&home);
        assert!(seeded.none_installed());
        let words = nothing_installed(&seeded);
        assert!(words.contains("no detection manifests in"), "{words}");
        assert!(words.contains("no message can be delivered"), "{words}");
        assert!(words.contains("cyclops start again"), "{words}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_linked_manifest_directory_is_refused_without_touching_its_target() {
        let home = cyclops_proto::scratch::scratch_dir("cyc-manifest-link-home");
        let external = cyclops_proto::scratch::scratch_dir("cyc-manifest-link-external");
        for path in [&home, &external] {
            let _ = std::fs::remove_dir_all(path);
            std::fs::create_dir_all(path).unwrap();
        }
        let target = external.join("claude.toml");
        std::fs::write(&target, b"external\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&external, dir(&home)).unwrap();

        let seeded = seed(&home);

        assert!(seeded.written.is_empty());
        assert!(!seeded.problems.is_empty());
        assert_eq!(std::fs::read(&target).unwrap(), b"external\n");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&external);
    }
}
