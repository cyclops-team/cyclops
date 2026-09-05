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
//! Writing never overwrites an existing file. These files are meant to be
//! edited, and even a known old Cyclops seed is left where the operator can
//! review it. [`EVER_SHIPPED_FNV64`] records that known-old state without
//! granting replacement authority.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use cyclops_state::{CreateFileOutcome, StateInspector, StateRoot};

use crate::hash::fnv64;

/// Every manifest the binary carries, by file name.
const SHIPPED: &[(&str, &str)] = &[
    (
        "adal.toml",
        include_str!("../../../resources/manifests/adal.toml"),
    ),
    (
        "agy.toml",
        include_str!("../../../resources/manifests/agy.toml"),
    ),
    (
        "aider.toml",
        include_str!("../../../resources/manifests/aider.toml"),
    ),
    (
        "amp.toml",
        include_str!("../../../resources/manifests/amp.toml"),
    ),
    (
        "auggie.toml",
        include_str!("../../../resources/manifests/auggie.toml"),
    ),
    (
        "autohand.toml",
        include_str!("../../../resources/manifests/autohand.toml"),
    ),
    (
        "bob.toml",
        include_str!("../../../resources/manifests/bob.toml"),
    ),
    (
        "claude.toml",
        include_str!("../../../resources/manifests/claude.toml"),
    ),
    (
        "cline.toml",
        include_str!("../../../resources/manifests/cline.toml"),
    ),
    (
        "codearts.toml",
        include_str!("../../../resources/manifests/codearts.toml"),
    ),
    (
        "codebuddy.toml",
        include_str!("../../../resources/manifests/codebuddy.toml"),
    ),
    (
        "codex.toml",
        include_str!("../../../resources/manifests/codex.toml"),
    ),
    (
        "commandcode.toml",
        include_str!("../../../resources/manifests/commandcode.toml"),
    ),
    (
        "continue.toml",
        include_str!("../../../resources/manifests/continue.toml"),
    ),
    (
        "copilot.toml",
        include_str!("../../../resources/manifests/copilot.toml"),
    ),
    (
        "cortex.toml",
        include_str!("../../../resources/manifests/cortex.toml"),
    ),
    (
        "crush.toml",
        include_str!("../../../resources/manifests/crush.toml"),
    ),
    (
        "cursor.toml",
        include_str!("../../../resources/manifests/cursor.toml"),
    ),
    (
        "dcode.toml",
        include_str!("../../../resources/manifests/dcode.toml"),
    ),
    (
        "devin.toml",
        include_str!("../../../resources/manifests/devin.toml"),
    ),
    (
        "dexto.toml",
        include_str!("../../../resources/manifests/dexto.toml"),
    ),
    (
        "droid.toml",
        include_str!("../../../resources/manifests/droid.toml"),
    ),
    (
        "forge.toml",
        include_str!("../../../resources/manifests/forge.toml"),
    ),
    (
        "gemini.toml",
        include_str!("../../../resources/manifests/gemini.toml"),
    ),
    (
        "goose.toml",
        include_str!("../../../resources/manifests/goose.toml"),
    ),
    (
        "grok.toml",
        include_str!("../../../resources/manifests/grok.toml"),
    ),
    (
        "hermes.toml",
        include_str!("../../../resources/manifests/hermes.toml"),
    ),
    (
        "iflow.toml",
        include_str!("../../../resources/manifests/iflow.toml"),
    ),
    (
        "jazz.toml",
        include_str!("../../../resources/manifests/jazz.toml"),
    ),
    (
        "junie.toml",
        include_str!("../../../resources/manifests/junie.toml"),
    ),
    (
        "kilo.toml",
        include_str!("../../../resources/manifests/kilo.toml"),
    ),
    (
        "kimchi.toml",
        include_str!("../../../resources/manifests/kimchi.toml"),
    ),
    (
        "kimi.toml",
        include_str!("../../../resources/manifests/kimi.toml"),
    ),
    (
        "kiro.toml",
        include_str!("../../../resources/manifests/kiro.toml"),
    ),
    (
        "kode.toml",
        include_str!("../../../resources/manifests/kode.toml"),
    ),
    (
        "loaf.toml",
        include_str!("../../../resources/manifests/loaf.toml"),
    ),
    (
        "mcode.toml",
        include_str!("../../../resources/manifests/mcode.toml"),
    ),
    (
        "neovate.toml",
        include_str!("../../../resources/manifests/neovate.toml"),
    ),
    (
        "openclaw.toml",
        include_str!("../../../resources/manifests/openclaw.toml"),
    ),
    (
        "opencode.toml",
        include_str!("../../../resources/manifests/opencode.toml"),
    ),
    (
        "openhands.toml",
        include_str!("../../../resources/manifests/openhands.toml"),
    ),
    (
        "pa.toml",
        include_str!("../../../resources/manifests/pa.toml"),
    ),
    (
        "pi.toml",
        include_str!("../../../resources/manifests/pi.toml"),
    ),
    (
        "qoder.toml",
        include_str!("../../../resources/manifests/qoder.toml"),
    ),
    (
        "qodercn.toml",
        include_str!("../../../resources/manifests/qodercn.toml"),
    ),
    (
        "qwen.toml",
        include_str!("../../../resources/manifests/qwen.toml"),
    ),
    (
        "reasonix.toml",
        include_str!("../../../resources/manifests/reasonix.toml"),
    ),
    (
        "rovodev.toml",
        include_str!("../../../resources/manifests/rovodev.toml"),
    ),
    (
        "tabnine.toml",
        include_str!("../../../resources/manifests/tabnine.toml"),
    ),
    (
        "traecli.toml",
        include_str!("../../../resources/manifests/traecli.toml"),
    ),
    (
        "vibe.toml",
        include_str!("../../../resources/manifests/vibe.toml"),
    ),
    (
        "warp.toml",
        include_str!("../../../resources/manifests/warp.toml"),
    ),
];

/// A manifest is small configuration, not an unbounded data channel. A plan
/// that cannot inspect a complete regular file requires manual review rather
/// than allocating or guessing its ownership.
const PLAN_FILE_BYTES_LIMIT: usize = 1024 * 1024;

/// Where manifests go, and where cyclopsd looks with no `manifest_dir` set.
pub fn dir(home: &Path) -> PathBuf {
    home.join("manifests")
}

/// FNV-1a 64 of every manifest body this project has ever shipped, the
/// current ones included. A file on disk whose hash is in this list is a
/// known old Cyclops seed rather than an arbitrary operator edit. Both kinds
/// of existing file remain untouched.
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
/// seeded with those, so listing one would misclassify bytes that could just
/// as well be an operator's own. And hashes from any other seeded file: this
/// list answers whether this manifest is a known old Cyclops seed, and a
/// skill body's hash landing here would misclassify an edited manifest.
const EVER_SHIPPED_FNV64: &[&str] = &[
    "000da241c916d3cb",
    "07e6faf978ea2598",
    "0a90724a6ccb9c0a",
    "0d86f7b98bf8baa1",
    "0e7c902fb67d702a",
    "0ea59360ef2c838f",
    "0f1165768798c296",
    "11524da667efff46",
    "12765e334728f40b",
    "16f1cc714b44548c",
    "192c1584efd10f6f",
    "1bc8d26b815b93e0",
    "21a94a206aefa357",
    "22fc7137a759b208",
    "28390246d6b96f16",
    "2893de300e4fc944",
    "2949a1d316e76d5b",
    "2a8c5b29dff89435",
    "2ab0cdc3cc03647b",
    "2d760e8dbff7a191",
    "3136ad89b6b415f1",
    "316b4c51088fc315",
    "327573d09f05ed65",
    "3292eae226f54f79",
    "32cbf16f4ecde8e9",
    "36135e553251b3d9",
    "3718f2ac011587c4",
    "3860125b31455235",
    "38c8d60f54914523",
    "39e895dafb8bedb7",
    "3a7c5a91fee57051",
    "3af7a3dfe7e971f7",
    "3e1f2f2587609500",
    "3e966f9f7049d206",
    "3ec9936975418c27",
    "3ee05959c01113a7",
    "3f2860fa1275d798",
    "447ac3d6d4fb9127",
    "44c3c94fe03c18c9",
    "4af4029b9d2910e3",
    "4b643cdbc4b91014",
    "4cc9bf79e99d7d25",
    "4f222235de989aea",
    "535f5df99678a1bb",
    "57c2bf8d8b894ce0",
    "5be3ed83f3683522",
    "5fb9fab4521686ad",
    "645013f86f1bcb41",
    "6680bd20b0f93e56",
    "68b6a34d1a938aa0",
    "69c853cdcc84e970",
    "6a4f11941f4c78be",
    "6acd399933dc8d62",
    "6dd87d7438712329",
    "6f824b54f50c63b5",
    "70c53961fd4a59d1",
    "75f3c5daed1304f1",
    "76f7d149eba23837",
    "7a9754b750109c5e",
    "7de414306cccebc3",
    "7fc779fefde1e467",
    "81e7e3ad63748a7f",
    "821ec0a1af0f8cc1",
    "83b76b1edc0bcde7",
    "84b0dfa8ce699bb0",
    "84c43e4255939728",
    "85f276e9afffc42d",
    "86f0219077c6bae3",
    "884a6e869dbdea99",
    "8e524693400048f8",
    "8ffc2859ff8be0c4",
    "92e40157e2ad9b86",
    "970aabc764b3acd4",
    "97778c580f34b125",
    "98506601b0664b8a",
    "98ae3fa4eca78cf3",
    "9947c2cf75719a69",
    "9a67be48357ff9e0",
    "9be123836a9dac67",
    "9e4d60c2da88e88e",
    "9ebb59d89922a438",
    "9eee760afa8f5ead",
    "ae6a3ab16e540024",
    "b08adf7a1bf1d116",
    "b327aa8824a51b94",
    "b4ef7ddba83f94a1",
    "b689983b65dadf94",
    "b762e1138fd599c7",
    "b92c9f9c63314d16",
    "baa8b17e1e28dc2c",
    "bc4bc371b40e71ca",
    "bf50a35c755a593c",
    "bf66db73f5d56a8f",
    "c5c15fada96a238f",
    "c60d4aff7fe2a5d2",
    "c74b7d8b4e445057",
    "c86ba6c35e662ee6",
    "ca76bd51cbb6ff12",
    "cd2508367907b954",
    "cdc9378b2adb994e",
    "d184c3a8a39327f5",
    "d4a0c49fff22fe23",
    "d4c5172af411c75f",
    "d97156863fecc950",
    "da723b27c017b7a5",
    "dc0b06781eb5a812",
    "dcf8e732f46b397c",
    "df257d8320645462",
    "e2a3e259f46c4dfc",
    "e49a078f35012c20",
    "e75a04594790f80a",
    "e8f09401778d50a0",
    "e9b09acc9f7a0390",
    "eba911263fbb7fed",
    "ed6a0d16eff01e23",
    "ee41a0a36c59c466",
    "f0cbb861e241aae1",
    "f10521646e776817",
    "f30d5c96c302f04b",
    "f519b6f9f9a6cbb4",
    "f6c7c7aaa830babb",
    "f7e41c33373cd344",
    "f88dc6a7f946f0bd",
    "fa49d370de55976e",
    "ffb04e7dffbace31",
];

/// True when these bytes are a manifest this project released, so setup can
/// report an outdated seed without calling it an operator edit.
pub(crate) fn unedited_seed(body: &[u8]) -> bool {
    EVER_SHIPPED_FNV64.contains(&fnv64(body).as_str())
}

/// Every compiled-in manifest as (file name, body), in shipped order. The
/// vendor catalog (`crate::wiring`) parses these once per process.
pub(crate) fn shipped() -> impl Iterator<Item = (&'static str, &'static str)> {
    SHIPPED.iter().copied()
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
    /// Shipped files created this run because they were missing.
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

/// One body-free preview of a manifest seed destination.
///
/// The target is always one compiled-in manifest. The decision says whether
/// setup may create that target without exposing its contents.
pub struct PlannedManifest {
    pub path: PathBuf,
    pub decision: crate::managed_assets::SeedDecision,
}

/// Preview the manifest work `cyclops start --setup-only` would consider.
///
/// This opens an existing home only. A missing or unsafe root does not become
/// a directory merely because someone asked for a plan.
pub fn plan(home: &Path) -> Vec<PlannedManifest> {
    // `StateRoot::open_existing` may repair a state-root mode. A preview must
    // not repair anything, so it deliberately uses the inspection-only root.
    let root = StateInspector::open_existing(home);
    SHIPPED
        .iter()
        .map(|(name, body)| {
            let descendant = Path::new("manifests").join(name);
            let decision = match &root {
                Ok(Some(root)) => match planned_decision_at(root, &descendant, body.as_bytes()) {
                    Ok(decision) => decision,
                    Err(_) => crate::managed_assets::refuse_unreadable_or_unproven(),
                },
                Ok(None) => {
                    crate::managed_assets::seed_decision(None, body.as_bytes(), unedited_seed)
                }
                Err(_) => crate::managed_assets::refuse_unreadable_or_unproven(),
            };
            PlannedManifest {
                path: dir(home).join(name),
                decision,
            }
        })
        .collect()
}

/// Read through the inspection-only root for `setup plan`. A truncated,
/// linked, unstable, or otherwise unreadable target is intentionally left
/// untouched.
fn planned_decision_at(
    root: &StateInspector,
    descendant: &Path,
    current: &[u8],
) -> Result<crate::managed_assets::SeedDecision, ()> {
    match root.read_file(descendant, PLAN_FILE_BYTES_LIMIT) {
        Ok(Some(file)) if !file.truncated => Ok(crate::managed_assets::seed_decision(
            Some(&file.bytes),
            current,
            unedited_seed,
        )),
        Ok(None) => Ok(crate::managed_assets::seed_decision(
            None,
            current,
            unedited_seed,
        )),
        Ok(Some(_)) | Err(_) => Err(()),
    }
}

/// Read one manifest through the same held-root boundary the writer uses,
/// then hand the byte ownership question to `managed_assets`.
fn seed_decision_at(
    root: &StateRoot,
    descendant: &Path,
    current: &[u8],
) -> Result<crate::managed_assets::SeedDecision, String> {
    let existing = match root.open_read(descendant) {
        Ok(Some(mut file)) => {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|e| format!("read {}: {e}", root.path().join(descendant).display()))?;
            Some(bytes)
        }
        Ok(None) => None,
        Err(e) => {
            return Err(format!(
                "read {}: {e}",
                root.path().join(descendant).display()
            ))
        }
    };
    Ok(crate::managed_assets::seed_decision(
        existing.as_deref(),
        current,
        unedited_seed,
    ))
}

/// Put missing shipped manifests in `<home>/manifests`, keeping every file
/// that is already there.
///
/// Runs on every `cyclops start`, not only the first: a release that adds a
/// manifest reaches an existing home without a reinstall. An existing file,
/// including a known old body listed in [`EVER_SHIPPED_FNV64`], remains
/// byte-for-byte in place. The plan and setup check report that old state so
/// an operator can review it deliberately.
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
        let decision = match seed_decision_at(&root, &descendant, body.as_bytes()) {
            Ok(decision) => decision,
            Err(problem) => {
                out.problems.push(problem);
                continue;
            }
        };
        match decision.action() {
            crate::managed_assets::SeedAction::Create => {
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
            crate::managed_assets::SeedAction::KeepCurrent
            | crate::managed_assets::SeedAction::PreserveKnownOldSeed
            | crate::managed_assets::SeedAction::PreserveOperatorEdit => {
                out.kept.push((*name).to_string());
            }
            crate::managed_assets::SeedAction::RefuseUnreadableOrUnproven => {
                out.problems.push(format!(
                    "refuse {}: {}",
                    path.display(),
                    decision.ownership_reason()
                ));
            }
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
/// One exception keeps a known old released seed from shadowing the shipped
/// fallback for the same id. The file remains untouched on disk, but the
/// current compiled-in manifest answers this lookup. A home file whose id
/// the shipped set does not carry is kept either way: that file is still
/// what cyclopsd reads.
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

    /// Every current shipped body must be in the ever-shipped hash list, so
    /// a later setup check can recognize it as current rather than edited.
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

    /// A known old seed is an observable setup condition, not permission to
    /// rewrite the file. A local released body under another shipped name
    /// gives this regression an always-available known-old value.
    #[test]
    fn an_existing_known_old_seed_is_preserved_byte_for_byte() {
        use std::os::unix::fs::MetadataExt as _;

        let home = cyclops_proto::scratch::scratch_dir("cyc-seed-known-old");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(dir(&home)).expect("create manifests directory");
        let claude = dir(&home).join("claude.toml");
        std::fs::write(&claude, shipped("codex.toml")).expect("plant");
        let before = std::fs::metadata(&claude).expect("known-old metadata");
        let before_bytes = std::fs::read(&claude).expect("known-old bytes");

        let seeded = seed(&home);
        assert!(!seeded.written.contains(&"claude.toml".to_string()));
        assert!(seeded.kept.contains(&"claude.toml".to_string()));
        let after = std::fs::metadata(&claude).expect("known-old metadata after setup");
        assert_eq!(after.dev(), before.dev());
        assert_eq!(after.ino(), before.ino());
        assert_eq!(std::fs::read(&claude).unwrap(), before_bytes);
        assert!(
            unedited_seed(&before_bytes),
            "the local regression body must stay classified as a known old seed"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The other half, and the one `--agents` actually hits first.
    /// `cyclops start` resolves the agent list before it seeds. A known old
    /// file stays on disk, so the compiled-in manifest must answer this
    /// lookup for the same id rather than letting the old file shadow it.
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
