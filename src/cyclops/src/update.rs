//! `cyclops update`: replace the installed build with a fresh one.
//!
//! Reinstalling was always the update path: `scripts/install.sh` builds,
//! copies beside the installed binaries and renames over them, and
//! rewrites a file already in the home only when Cyclops wrote it and can
//! prove it. Config is never rewritten; the theme and manifest seeds
//! replace only bytes they hash as their own, and the hook refresh only
//! artifacts its own receipt vouches for. What was missing was the verb,
//! a before-and-after report, and the restart steps, so updating meant
//! re-finding the curl one-liner and guessing whether it took. This verb
//! owns those three things and delegates the install itself to the
//! installer, so "get a new build onto this machine" keeps exactly one
//! implementation.
//!
//! The source is a fresh clone of `CYCLOPS_REPO` at `CYCLOPS_REF` (the
//! installer's own overrides, same defaults), never the hosted
//! install.sh: the hosted copy is only as fresh as the last website
//! deploy, and a clone always runs the installer that matches the code
//! it builds.
//!
//! Nothing is restarted here. The daemon and any open workspace keep
//! executing the replaced binary until they exit, which is why the last
//! thing printed is the three steps that put the new build in charge.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::copy;
use crate::render;
use crate::style::Style;

/// The installer's defaults (scripts/install.sh:26-27), restated because
/// the binary cannot read that file at runtime: change them together.
const DEFAULT_REPO: &str = "https://github.com/cyclops-team/cyclops.git";
const DEFAULT_REF: &str = "main";

/// The commit baked into this binary by build.rs.
const BUILD_REF: &str = env!("CYCLOPS_BUILD_REF");

/// What the baked build ref can say about a freshness check.
#[derive(Debug, PartialEq)]
enum LocalBuild {
    /// A clean commit: comparable to the remote by sha prefix.
    Sha(String),
    /// Built from edited sources; no remote commit can match it.
    Dirty(String),
    /// Built outside git (a source tarball); there is nothing to compare.
    Unknown,
}

/// build.rs stamps exactly three shapes: `<short-sha>`, `<short-sha>.dirty`,
/// or the literal `unknown`.
fn classify(build_ref: &str) -> LocalBuild {
    if build_ref == "unknown" {
        LocalBuild::Unknown
    } else if build_ref.ends_with(".dirty") {
        // The full form is kept: the note quotes it, and the bare sha is
        // never compared (nothing can match an edited tree).
        LocalBuild::Dirty(build_ref.to_string())
    } else {
        LocalBuild::Sha(build_ref.to_string())
    }
}

/// The remote already carries the running commit. Prefix match, because
/// build.rs stamps `git rev-parse --short`, whose length grows when short
/// shas collide, and the remote answers with the full 40.
fn is_current(local_short: &str, remote_sha: &str) -> bool {
    !local_short.is_empty()
        && remote_sha.len() >= local_short.len()
        && remote_sha.starts_with(local_short)
}

/// The commit `reff` names at `repo`, asked of the remote itself. One
/// cheap round trip, no clone, no checkout.
fn ls_remote(repo: &str, reff: &str) -> Result<String, String> {
    let out = Command::new("git")
        .args(["ls-remote", repo, reff])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    match text.split_whitespace().next() {
        Some(sha) if !sha.is_empty() => Ok(sha.to_string()),
        _ => Err(format!("{reff} names nothing there")),
    }
}

/// Shallow clone of `repo` at `reff` into `dest`, quiet on success.
///
/// `HEAD` cannot go through `--branch` (it is not a branch), so it takes
/// the bare clone, which lands on whatever the repo has checked out. That
/// is the form that lets a local mirror with no named branches serve as
/// an update source.
fn clone(repo: &str, reff: &str, dest: &Path) -> Result<(), String> {
    let mut cmd = Command::new("git");
    cmd.args(["clone", "--depth", "1"]);
    if reff != "HEAD" {
        cmd.args(["--branch", reff]);
    }
    let out = cmd
        .arg(repo)
        .arg(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Where update keeps its build cache, so a rebuild is incremental.
///
/// Under the cyclops home rather than the system cache directory: it is
/// this tool's own scratch, an operator looking for disk knows to look
/// here, and it is removed with the home by the uninstaller.
fn build_cache() -> PathBuf {
    cyclops_proto::cyclops_home().join("build-cache")
}

/// The clone's throwaway directory, removed however `run` returns.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The installed binary, resolved the way a shell resolves it.
///
/// Never this process, for two different reasons. Reporting the new
/// version: this process is still the old build, and self-reporting is how
/// an update that silently failed would go unnoticed. Picking the prefix:
/// the copy a shell runs is the copy an update has to land on, and this
/// process may be a build directory nobody installed from.
fn installed_cyclops() -> Option<PathBuf> {
    if let Some(p) = which("cyclops") {
        return Some(p);
    }
    // Nothing on PATH: the installer's own prefix candidates, in its
    // order, for the case where it just created the directory and only a
    // fresh shell will see it.
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    [".local/bin", "bin", ".cargo/bin"]
        .iter()
        .map(|d| home.join(d).join("cyclops"))
        .find(|p| p.is_file())
}

/// First match for a bare name on PATH. daemon.rs holds the same six
/// lines for finding cyclopsd; the two resolve different binaries for
/// different reasons and neither is a library.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Where the installer must write, resolved before anything is replaced.
///
/// None when no cyclops resolves at all, which is the machine that has
/// never been installed to; there the installer's own `pick_prefix` is the
/// only answer and it makes it.
fn install_prefix() -> Option<PathBuf> {
    installed_cyclops()?.parent().map(Path::to_path_buf)
}

/// `<bin> --version` with the leading command name stripped, so the
/// report reads `0.1.0 (e610afc)` on both sides of the arrow.
fn version_of(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(version_words(&text))
}

fn version_words(version_line: &str) -> String {
    version_line
        .strip_prefix("cyclops ")
        .unwrap_or(version_line)
        .to_string()
}

/// The already-current badge. Heavy check by render::check's rule: the
/// remote owns the fact and just answered for it.
fn current_badge(reff: &str, style: &Style) -> String {
    format!(
        "{} {} {}",
        style.bold(&format!(
            "{} already the latest {reff}",
            render::check(true)
        )),
        style.dim("·"),
        style.dim("nothing to update")
    )
}

/// The old-to-new report. Heavy check: the new binary on disk answered
/// `--version` itself.
fn updated_badge(old: &str, new: &str, style: &Style) -> String {
    format!(
        "{} {} {old} → {new}",
        style.bold(&format!("{} updated", render::check(true))),
        style.dim("·")
    )
}

/// The three steps that put the new build in charge, in the grid shape
/// `cyclops start` prints its `Next:` block in. Exactly three, and never
/// run for the user: stopping the daemon out from under an open
/// workspace mid-session is the one thing an update must not do on its
/// own. Labeled `Restart:` because the installer's stream above already
/// printed a `Next:` of its own.
fn restart_steps(style: &Style) -> String {
    let steps: [(&str, &str); 3] = [
        ("q", "quit any open workspace; it is still on the old build"),
        ("cyclops daemon stop", "the daemon is too"),
        ("cyclops start", "come back up on the new build"),
    ];
    let w = steps
        .iter()
        .map(|(cmd, _)| cyclops_ui::grid::display_width(cmd))
        .max()
        .unwrap_or(0);
    let mut out = vec!["Restart:".to_string()];
    for (i, (cmd, why)) in steps.iter().enumerate() {
        out.push(format!(
            "  {}  {}  {}",
            style.dim(&format!("{}", i + 1)),
            render::pad(cmd, w),
            style.dim(why)
        ));
    }
    out.join("\n")
}

/// `cyclops update`. Steps, in the order that keeps a no-op cheap:
///
/// 1. Say which build is running and where the update comes from.
/// 2. Freshness check: `git ls-remote` against the baked sha. Already
///    current says so and stops; a build that cannot be compared says why
///    and goes on.
/// 3. Clone and run the clone's installer, streaming its output, at the
///    prefix the current install already uses, and building into a cache
///    that outlives the clone so the compile is incremental. Its last step
///    is `cyclops start --setup-only --wire-hooks`, which writes the hook
///    config each installed agent CLI reads and repoints the prepared
///    artifacts at the new binary.
/// 4. Report old to new, from the new binary's own `--version`, then the
///    restart steps.
pub fn run(json: bool, style: &Style) -> i32 {
    if json {
        eprintln!("{}", copy::UPDATE_NO_JSON);
        return crate::EXIT_USAGE;
    }
    let repo = env_or("CYCLOPS_REPO", DEFAULT_REPO);
    let reff = env_or("CYCLOPS_REF", DEFAULT_REF);

    // 1.
    println!("cyclops {}", crate::VERSION);
    println!("  {}", style.dim(&format!("source {repo} at {reff}")));

    // 2.
    match classify(BUILD_REF) {
        LocalBuild::Sha(sha) => match ls_remote(&repo, &reff) {
            Ok(remote) if is_current(&sha, &remote) => {
                println!("{}", current_badge(&reff, style));
                return 0;
            }
            Ok(_) => {}
            Err(cause) => {
                eprintln!("{}", copy::update_unreachable(&repo, &reff, &cause));
                return 1;
            }
        },
        LocalBuild::Dirty(build_ref) => {
            println!("  {}", style.dim(&copy::update_dirty(&build_ref)));
        }
        LocalBuild::Unknown => println!("  {}", style.dim(copy::UPDATE_UNKNOWN)),
    }

    // 3. Runtime path, not test scratch, same as the installer's own
    //    mktemp under ${TMPDIR:-/tmp}.
    let scratch =
        Scratch(std::env::temp_dir().join(format!("cyclops-update.{}", std::process::id())));
    if let Err(e) = std::fs::create_dir_all(&scratch.0) {
        eprintln!(
            "{}",
            copy::update_clone_failed(&repo, &reff, &e.to_string())
        );
        return 1;
    }
    let src = scratch.0.join("cyclops");
    if let Err(cause) = clone(&repo, &reff, &src) {
        eprintln!("{}", copy::update_clone_failed(&repo, &reff, &cause));
        return 1;
    }
    // The clone's installer, streamed: it prints every file it touches,
    // and it is the one implementation of where binaries and home live.
    //
    // --prefix pins it to the directory the install already uses. With no
    // --prefix the installer re-picks from the current PATH, so a PATH
    // that changed since install writes the new build somewhere else,
    // leaves the old one in place, and every hook already wired keeps
    // invoking it. A stale build answering hooks is worse than a missing
    // one: the edges keep arriving, from a build the daemon no longer
    // matches. Where it lands is printed, because a prefix is the kind of
    // thing an operator only discovers is wrong much later.
    let mut installer = Command::new("sh");
    installer.arg(src.join("scripts").join("install.sh"));
    // Build into a directory that outlives the clone.
    //
    // The clone is a fresh temp dir every run, so cargo's target/ starts
    // empty and a one-commit change pays for the whole dependency tree,
    // roughly 130 crates. Then `Scratch` deletes the clone on the way out,
    // taking the build with it, so the next update starts cold as well.
    // install.sh already reads CARGO_TARGET_DIR; nothing ever set it.
    //
    // Pointing it somewhere persistent makes cargo do what cargo does and
    // recompile only what moved. An operator who set their own target dir
    // keeps it: they have already decided where builds go.
    let cache = build_cache();
    if std::env::var_os("CARGO_TARGET_DIR").is_none() {
        match std::fs::create_dir_all(&cache) {
            Ok(()) => {
                installer.env("CARGO_TARGET_DIR", &cache);
                println!("  {}", style.dim(&copy::update_build_cache(&cache)));
            }
            // Not fatal. A cache that cannot be made costs a slow build,
            // which is what every update did before this existed.
            Err(e) => eprintln!("  {}", style.dim(&copy::update_cache_unusable(&cache, &e))),
        }
    }
    if let Some(prefix) = install_prefix() {
        println!(
            "  {}",
            style.dim(&format!("installing over {}", prefix.display()))
        );
        installer.arg("--prefix").arg(prefix);
    }
    match installer.status() {
        Ok(s) if s.success() => {}
        Ok(_) => {
            eprintln!("{}", copy::update_install_failed(None));
            return 1;
        }
        Err(e) => {
            eprintln!("{}", copy::update_install_failed(Some(&e.to_string())));
            return 1;
        }
    }

    // 4.
    println!();
    match installed_cyclops().and_then(|p| version_of(&p)) {
        Some(new) => println!("{}", updated_badge(crate::VERSION, &new, style)),
        None => println!("  {}", style.dim(copy::UPDATE_UNRESOLVED)),
    }
    println!();
    println!("{}", restart_steps(style));
    0
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three shapes build.rs stamps, and nothing else. `.dirty` and
    /// `unknown` must never reach the sha compare: the first can never
    /// match and the second has nothing to match with.
    #[test]
    fn build_refs_classify_into_the_three_stamped_shapes() {
        assert_eq!(classify("e610afc"), LocalBuild::Sha("e610afc".into()));
        assert_eq!(
            classify("e610afc.dirty"),
            LocalBuild::Dirty("e610afc.dirty".into())
        );
        assert_eq!(classify("unknown"), LocalBuild::Unknown);
    }

    /// Prefix match against the remote's full sha, because the baked side
    /// is `--short` and its length is git's choice, not ours.
    #[test]
    fn currency_is_a_prefix_match_on_the_short_sha() {
        let remote = "e610afc0123456789abcdef0123456789abcdef0";
        assert!(is_current("e610afc", remote));
        assert!(is_current("e610afc012", remote));
        assert!(!is_current("a1b2c3d", remote));
        // A sha longer than the remote's cannot match, and an empty local
        // sha must never read as current.
        assert!(!is_current(&"e".repeat(41), remote));
        assert!(!is_current("", remote));
        // A .dirty ref never reaches this compare, but if one did the
        // suffix keeps it from matching.
        assert!(!is_current("e610afc.dirty", remote));
    }

    #[test]
    fn the_report_strips_the_command_name_from_a_version_line() {
        assert_eq!(version_words("cyclops 0.1.0 (e610afc)"), "0.1.0 (e610afc)");
        // A shape from some other build is passed through rather than
        // half-eaten.
        assert_eq!(version_words("0.2.0 (abc1234)"), "0.2.0 (abc1234)");
    }

    #[test]
    fn the_badges_read_plain() {
        let plain = Style::none();
        assert_eq!(
            current_badge("main", &plain),
            "✔ already the latest main · nothing to update"
        );
        assert_eq!(
            updated_badge("0.1.0 (a1b2c3d)", "0.1.0 (e4f5a6b)", &plain),
            "✔ updated · 0.1.0 (a1b2c3d) → 0.1.0 (e4f5a6b)"
        );
    }

    /// Exactly three steps, in the only order that works: a stopped
    /// daemon under a still-open workspace leaves the workspace blind,
    /// so the workspace goes first.
    #[test]
    fn the_restart_steps_are_three_and_ordered() {
        let got = restart_steps(&Style::none());
        let expected = "Restart:\n\
                        \x20 1  q                    quit any open workspace; it is still on the old build\n\
                        \x20 2  cyclops daemon stop  the daemon is too\n\
                        \x20 3  cyclops start        come back up on the new build";
        assert_eq!(got, expected);
    }
}
