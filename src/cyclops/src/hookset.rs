//! `cyclops hooks`: install (render vendor hook configs), refresh (keep
//! the prepared ones pointed at this build), verify (hook liveness),
//! selftest (one no-op round trip through the delivery pipeline).
//!
//! Install PREPARES artifacts and prints wiring instructions; it never
//! writes into vendor dot-dirs (~/.claude, ~/.codex, ~/.gemini, .agents,
//! .cursor).
//! Configuration does not equal subscription:
//! a rendered config proves nothing until `hooks verify` or
//! `hooks selftest` shows edges actually arriving.
//!
//! Refresh is a different act from install and keeps the same boundary.
//! It rewrites bytes Cyclops itself wrote, at a path Cyclops chose, under
//! `$CYCLOPS_HOME/hooks/`, and only when the receipt beside them proves
//! it. Nothing it writes has any runtime effect until a human copies it,
//! which is why it needs no consent that install has not already had.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::ValueEnum;
use cyclops_proto::{DeliveryReceipt, DeliveryState};
use cyclops_state::StateRoot;
use serde_json::json;

use crate::client::Client;
use crate::hash::fnv64;
use crate::render::{display_width, human_duration, pad, receipt_badge};
use crate::style::Style;
use crate::{copy, EXIT_USAGE};

/// Templates ship inside the binary; the files under resources/hooks/ in
/// the repo are the source of truth and the golden tests hold the two
/// together.
const CLAUDE_TMPL: &str = include_str!("../../../resources/hooks/claude/settings.json.tmpl");
const CODEX_TMPL: &str = include_str!("../../../resources/hooks/codex/hooks.json.tmpl");
const AGY_TMPL: &str = include_str!("../../../resources/hooks/agy/hooks.json.tmpl");
const CURSOR_TMPL: &str = include_str!("../../../resources/hooks/cursor/hooks.json.tmpl");

/// Read timeout for hooks.selftest: the daemon waits up to 10s for the
/// delivery to resolve, so the client waits a little longer.
const SELFTEST_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Path components that mark a vendor CLI's own config tree. Install
/// refuses to write anywhere inside one, whatever the --dest says.
const VENDOR_DIRS: &[&str] = &[".claude", ".codex", ".gemini", ".agents", ".cursor"];

/// The receipt install drops beside every artifact it prepares.
///
/// Without it a refresh has two options and both are wrong: guess that any
/// JSON at the path it would have used is its own, or never refresh at
/// all. The first silently reverts the operator's edits, which is the
/// failure manifests.rs:53-105 exists to prevent; the second leaves every
/// prepared artifact naming a binary path that moved.
const RECEIPT_NAME: &str = ".cyclops-prepared.json";

/// Sequence for same-directory temporary artifact names.
static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CliKind {
    Claude,
    Codex,
    Agy,
    Cursor,
}

impl CliKind {
    fn template(self) -> &'static str {
        match self {
            CliKind::Claude => CLAUDE_TMPL,
            CliKind::Codex => CODEX_TMPL,
            CliKind::Agy => AGY_TMPL,
            CliKind::Cursor => CURSOR_TMPL,
        }
    }

    /// Rendered file name in the destination directory.
    fn file_name(self) -> &'static str {
        match self {
            CliKind::Claude => "settings.json",
            CliKind::Codex | CliKind::Agy | CliKind::Cursor => "hooks.json",
        }
    }

    fn name(self) -> &'static str {
        match self {
            CliKind::Claude => "claude",
            CliKind::Codex => "codex",
            CliKind::Agy => "agy",
            CliKind::Cursor => "cursor",
        }
    }

    /// The inverse of [`CliKind::name`]. Refresh reads the vendor out of a
    /// directory name install created, so the two live side by side and
    /// cannot drift apart unnoticed.
    pub fn from_name(name: &str) -> Option<CliKind> {
        match name {
            "claude" => Some(CliKind::Claude),
            "codex" => Some(CliKind::Codex),
            "agy" => Some(CliKind::Agy),
            "cursor" => Some(CliKind::Cursor),
            _ => None,
        }
    }
}

/// Render a template: substitute {label} and {cyclops_bin}, strip the '#'
/// comment header so the output is plain vendor JSON.
pub fn render(kind: CliKind, label: &str, cyclops_bin: &str) -> String {
    let body: String = kind
        .template()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = body
        .replace("{label}", label)
        .replace("{cyclops_bin}", cyclops_bin);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// The name a shared config registers itself under.
const SHARED_NAME: &str = "cyclops";

/// [`render`] for a file that EVERY pane of one vendor reads.
///
/// The same template with the per-pane identity taken back out. codex reads
/// a single `$CODEX_HOME/hooks.json` and agy a single `~/.agents/hooks.json`,
/// so a label baked into either would make every pane report as one agent.
/// With no `--agent`, the daemon derives the reporter from the authenticated
/// socket peer. `CYCLOPS_AGENT` only namespaces the hook sequence counter.
///
/// Rendering then stripping, rather than a second set of templates, so the
/// events and payload shapes cannot drift between the two forms. agy's
/// named-hooks key keeps SHARED_NAME, which is a registration name and not
/// an agent label.
pub fn render_shared(kind: CliKind, cyclops_bin: &str) -> String {
    render(kind, SHARED_NAME, cyclops_bin).replace(&format!(" --agent {SHARED_NAME}"), "")
}

/// The default file this vendor reads hooks from when started normally.
///
/// Claude also accepts an explicit settings file at launch. That is useful
/// for panes Cyclops creates, but normal direct launches read
/// `~/.claude/settings.json`. Treating Claude as launch-only leaves those
/// sessions permanently without lifecycle hooks.
fn vendor_hook_file(kind: CliKind) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    match kind {
        CliKind::Claude => Some(home.join(".claude").join("settings.json")),
        // User level, and not the project-local alternative. MEASURED
        // Project-local .codex/hooks.json does not load until
        // the directory is trusted, and in a non-interactive run that
        // dialog can never be answered, so the hooks silently never fire.
        CliKind::Codex => Some(crate::consumer::root(kind, &home).join("hooks.json")),
        CliKind::Agy => Some(home.join(".agents").join("hooks.json")),
        CliKind::Cursor => Some(crate::consumer::root(kind, &home).join("hooks.json")),
    }
}

/// True for a hook entry this project wrote.
///
/// Identified by what the command runs rather than by a marker key: a
/// marker would have to survive the vendor rewriting its own config, and
/// some do. Any command that invokes a cyclops binary's `hook` receiver is
/// ours to replace; everything else in the file is the operator's and is
/// carried through untouched.
fn is_cyclops_entry(v: &serde_json::Value) -> bool {
    let text = v.to_string();
    text.contains(" hook ") && text.contains("cyclops")
}

/// Merge `src` into `dst`, replacing only this project's own entries.
///
/// Objects recurse so an unrelated sibling key is never visited. Arrays are
/// the case that matters: a vendor's event list holds the operator's
/// handlers next to ours, so ours are filtered out and re-appended while
/// theirs keep their order. That is what makes a second run a no-op instead
/// of a file that grows a duplicate handler every update.
fn merge_into(dst: &mut serde_json::Value, src: &serde_json::Value) {
    use serde_json::Value;
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, sv) in s {
                merge_into(d.entry(k.clone()).or_insert(Value::Null), sv);
            }
        }
        (d @ Value::Array(_), Value::Array(s)) => {
            let kept: Vec<Value> = d
                .as_array()
                .map(|a| a.iter().filter(|e| !is_cyclops_entry(e)).cloned().collect())
                .unwrap_or_default();
            *d = Value::Array(kept.into_iter().chain(s.iter().cloned()).collect());
        }
        (d, s) => *d = s.clone(),
    }
}

#[derive(Clone, Copy)]
pub(crate) enum WiringState {
    Missing,
    Current,
    NeedsUpdate,
    Invalid,
    Unreadable,
}

impl WiringState {
    pub(crate) fn word(self) -> &'static str {
        match self {
            WiringState::Missing => "missing",
            WiringState::Current => "current",
            WiringState::NeedsUpdate => "needs_update",
            WiringState::Invalid => "invalid",
            WiringState::Unreadable => "unreadable",
        }
    }

    pub(crate) fn ready(self) -> bool {
        matches!(self, WiringState::Current)
    }
}

pub(crate) struct WiringCheck {
    pub path: Option<PathBuf>,
    pub state: WiringState,
}

/// Evaluate hook wiring from bytes obtained by a caller-owned safe reader.
pub(crate) fn inspect_wiring_bytes(kind: CliKind, bytes: &[u8]) -> WiringState {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return WiringState::Unreadable;
    };
    let mut document: serde_json::Value = if text.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(text) {
            Ok(document) => document,
            Err(_) => return WiringState::Invalid,
        }
    };
    let before = document.clone();
    let expected = serde_json::from_str(&render_shared(kind, &cyclops_bin()))
        .expect("shipped hook template is valid JSON");
    merge_into(&mut document, &expected);
    if document == before {
        WiringState::Current
    } else {
        WiringState::NeedsUpdate
    }
}

/// Inspect fixed wiring by applying the current merge in memory.
pub(crate) fn inspect_wiring(kind: CliKind) -> WiringCheck {
    let Some(path) = vendor_hook_file(kind) else {
        return WiringCheck {
            path: None,
            state: WiringState::Unreadable,
        };
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return WiringCheck {
                path: Some(path),
                state: WiringState::Missing,
            };
        }
        Err(_) => {
            return WiringCheck {
                path: Some(path),
                state: WiringState::Unreadable,
            };
        }
    };
    WiringCheck {
        path: Some(path),
        state: inspect_wiring_bytes(kind, text.as_bytes()),
    }
}

/// What one vendor's wiring did, for the line `cyclops start` prints.
pub struct WiredVendor {
    pub vendor: &'static str,
    pub path: PathBuf,
    /// The file already said what this run would have written.
    pub unchanged: bool,
    /// Where the pre-existing file was copied before the first edit.
    pub backup: Option<PathBuf>,
}

/// Put this project's hook entries in the file `kind` reads on its own.
///
/// It writes into configuration this project does not own, so three rules hold and none is
/// optional: the operator's entries are merged around rather than replaced
/// ([`merge_into`]), the original is copied aside before the first edit, and
/// a run that would change nothing writes nothing at all.
///
/// Ok(None) means the vendor is not installed on this machine. That is not a
/// failure: writing a config for a CLI that is not here would leave a file
/// nobody reads.
pub fn wire_vendor(kind: CliKind) -> Result<Option<WiredVendor>, String> {
    let Some(path) = vendor_hook_file(kind) else {
        return Ok(None);
    };
    let dir = path.parent().ok_or("hook path has no parent")?;
    // AGY's generic .agents hook directory may belong to another consumer.
    // Its own CLI home is the install gate.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("vendor hook path requires HOME");
    let installed_root = crate::consumer::root(kind, &home);
    if !installed_root.is_dir() {
        return Ok(None);
    }
    if !dir.is_dir() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("can't create hook directory {}: {e}", dir.display()))?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&existing)
            .map_err(|e| format!("{} is not valid JSON ({e}); left alone", path.display()))?
    };
    let ours: serde_json::Value = serde_json::from_str(&render_shared(kind, &cyclops_bin()))
        .map_err(|e| {
            format!(
                "rendered {} hook config is not valid JSON: {e}",
                kind.name()
            )
        })?;

    let before = doc.clone();
    merge_into(&mut doc, &ours);
    if doc == before {
        return Ok(Some(WiredVendor {
            vendor: kind.name(),
            path,
            unchanged: true,
            backup: None,
        }));
    }

    // Copy aside before the first edit, and only then. A backup rewritten
    // on every run would eventually hold this project's own output and
    // stop being the thing the operator wanted back.
    let mut backup = None;
    if !existing.is_empty() {
        let bak = path.with_extension("json.before-cyclops");
        if !bak.exists() {
            std::fs::copy(&path, &bak).map_err(|e| {
                format!("can't back up {} to {}: {e}", path.display(), bak.display())
            })?;
        }
        backup = Some(bak);
    }
    let mut text = serde_json::to_string_pretty(&doc)
        .map_err(|e| format!("can't serialize {}: {e}", path.display()))?;
    text.push('\n');
    write_atomic(&path, &text).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    Ok(Some(WiredVendor {
        vendor: kind.name(),
        path,
        unchanged: false,
        backup,
    }))
}

/// Copy-pasteable wiring instructions for an isolated config artifact.
///
/// `hooks install` never changes vendor configuration. The separate
/// `start --setup-only --wire-hooks` path safely merges default config for
/// installed direct-launch CLIs after explicit consent.
fn instructions(kind: CliKind, rendered: &Path, label: &str) -> String {
    let p = rendered.display();
    match kind {
        CliKind::Claude => format!(
            "Use this isolated config with an explicit Claude launch:\n\
             \n\
             \x20 claude --settings {p}\n\
             \n\
             Already passing your own --settings file? Merge the \"hooks\" object\n\
             from {p} into it, preserving every unrelated setting and handler.\n\
             Do not replace the file.\n\
             \n\
             For normal direct Claude launches, run:\n\
             \n\
             \x20 cyclops start --setup-only --wire-hooks\n\
             \n\
             then restart Claude.\n\
             Then prove it fires: cyclops hooks selftest {label}"
        ),
        CliKind::Codex => format!(
            "Wire it without replacing shared config. Codex loads ZERO hooks in an\n\
             untrusted directory, and\n\
             --dangerously-bypass-hook-trust does NOT fix that.\n\
             \n\
             User-level hooks (no directory trust needed): if\n\
             ${{CODEX_HOME:-$HOME/.codex}}/hooks.json does not exist, copy {p} there.\n\
             If it already exists, merge only Cyclops' event entries from {p};\n\
             preserve every unrelated key and handler. Never overwrite it.\n\
             \n\
             After merging, open Codex's /hooks and review and trust the exact\n\
             Cyclops command definition. New or changed commands are skipped\n\
             until that exact definition is trusted. For project-local hooks,\n\
             also trust the project config layer in\n\
             ${{CODEX_HOME:-$HOME/.codex}}/config.toml (edit the path first):\n\
             \x20    [projects.\"/path/to/your/project\"]\n\
             \x20    trust_level = \"trusted\"\n\
             Reload behavior depends on the Codex version. If the running\n\
             process does not pick up the merged file or trust decision, restart\n\
             or reload Codex, then prove it fires with the selftest.\n\
             \n\
             Then prove it fires: cyclops hooks selftest {label}"
        ),
        CliKind::Agy => format!(
            "Wire it (agy reads .agents/hooks.json in the workspace it runs in):\n\
             \n\
             If <workspace>/.agents/hooks.json does not exist, copy {p} there.\n\
             If it already exists, merge only Cyclops' event entries from {p};\n\
             preserve every unrelated key and handler. Never overwrite it.\n\
             \n\
             agy has no payload-matchable acknowledgement: deliveries stay on the\n\
             screen-verified tier; these hooks feed liveness and turn detection.\n\
             Then check edges arrive: cyclops hooks verify {label}"
        ),
        CliKind::Cursor => format!(
            "Wire it (cursor reads hooks.json from the workspace it runs in, or\n\
             from your home directory):\n\
             \n\
             If <workspace>/.cursor/hooks.json does not exist, copy {p} there.\n\
             If it already exists, merge only Cyclops' event entries from {p};\n\
             preserve every unrelated key and handler. Never overwrite it.\n\
             The user-level alternative is ~/.cursor/hooks.json; apply the same\n\
             merge rule there.\n\
             \n\
             CURSOR_CONFIG_DIR does NOT work for hooks: it relocates\n\
             cli-config.json but hooks.json placed there fires zero events.\n\
             Then prove it fires: cyclops hooks selftest {label}"
        ),
    }
}

/// Write a prepared artifact without exposing a partially-written JSON file.
/// The temporary file is created beside the destination and renamed only after
/// its contents have been flushed to disk. An existing destination is replaced
/// by the single rename operation, so readers see either complete version.
fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hook");
    let pid = std::process::id();

    for _ in 0..32 {
        let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".{file_name}.tmp-{pid}-{seq}"));
        let mut file = match OpenOptions::new().create_new(true).write(true).open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };

        let result = file
            .write_all(content.as_bytes())
            .and_then(|_| file.sync_all())
            .and_then(|_| fs::rename(&temp, path));
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        return result;
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary hook artifact",
    ))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// What install recorded about one prepared artifact, written beside it as
/// [`RECEIPT_NAME`]. These five fields are what refresh compares; the file
/// also carries a timestamp and a build string for whoever opens it.
///
/// Hand-parsed rather than derived: this crate carries serde_json and not
/// serde, and five strings do not earn a dependency.
struct Receipt {
    vendor: String,
    agent: String,
    file: String,
    bin: String,
    rendered_fnv: String,
}

impl Receipt {
    fn path(dir: &Path) -> PathBuf {
        dir.join(RECEIPT_NAME)
    }

    /// None when there is no receipt, it is unreadable, or it is missing a
    /// field. Every one of those means the artifact beside it is not
    /// provably Cyclops', which is the same answer.
    #[cfg(test)]
    fn read(dir: &Path) -> Option<Receipt> {
        Self::parse(&fs::read(Self::path(dir)).ok()?)
    }

    fn read_owned(root: &StateRoot, dir: &Path) -> Result<Option<Receipt>, String> {
        let path = dir.join(RECEIPT_NAME);
        let Some(bytes) = read_owned(root, &path)? else {
            return Ok(None);
        };
        Ok(Self::parse(&bytes))
    }

    fn parse(bytes: &[u8]) -> Option<Receipt> {
        let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let field = |key: &str| value[key].as_str().map(String::from);
        Some(Receipt {
            vendor: field("vendor")?,
            agent: field("agent")?,
            file: field("file")?,
            bin: field("bin")?,
            rendered_fnv: field("rendered_fnv")?,
        })
    }

    /// Written after the artifact, never before. A crash between the two
    /// leaves an artifact with no receipt, which refresh skips; the other
    /// order would leave a receipt vouching for bytes nobody wrote.
    fn write(&self, dir: &Path) -> io::Result<()> {
        write_atomic(&Self::path(dir), &self.body())
    }

    fn write_owned(&self, root: &StateRoot, dir: &Path) -> Result<(), String> {
        let path = dir.join(RECEIPT_NAME);
        root.replace_file(&path, self.body().as_bytes())
            .map_err(|e| format!("can't write {}: {e}", root.path().join(path).display()))
    }

    fn body(&self) -> String {
        let body = json!({
            "vendor": self.vendor,
            "agent": self.agent,
            "file": self.file,
            "bin": self.bin,
            "rendered_fnv": self.rendered_fnv,
            "written_ms": now_ms(),
            "version": crate::VERSION,
        });
        format!("{body}\n")
    }
}

fn read_owned(root: &StateRoot, descendant: &Path) -> Result<Option<Vec<u8>>, String> {
    let path = root.path().join(descendant);
    let Some(mut file) = root
        .open_read(descendant)
        .map_err(|e| format!("can't read {}: {e}", path.display()))?
    else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("can't read {}: {e}", path.display()))?;
    Ok(Some(bytes))
}

/// True when `dest` points inside a vendor CLI's own config tree.
fn inside_vendor_dir(dest: &Path) -> bool {
    dest.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| VENDOR_DIRS.contains(&s))
    })
}

/// The path of this cyclops binary, for hook commands that must work from
/// any vendor CLI's environment without PATH assumptions.
fn cyclops_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "cyclops".to_string())
}

/// Where install writes with no `--dest`, and the only tree [`refresh`]
/// walks. An artifact placed anywhere else is outside refresh's reach, and
/// install says so rather than implying coverage.
fn hooks_root() -> PathBuf {
    hooks_root_in(&cyclops_proto::cyclops_home())
}

/// [`hooks_root`] for a home named explicitly, instead of the ambient one.
///
/// A caller that already resolved a home must not go back to the
/// environment for it. `cyclops start` holds one, and its tests run against
/// a scratch home: reading CYCLOPS_HOME there would write hook artifacts
/// into the operator's real home from a test run.
pub fn hooks_root_in(home: &Path) -> PathBuf {
    home.join("hooks")
}

pub fn run_install(
    kind: CliKind,
    label: &str,
    dry_run: bool,
    dest: Option<&Path>,
    json: bool,
) -> i32 {
    // The same rule the daemon enforces, from the same place, so the two
    // never disagree about which names exist (cyclops_proto::label).
    if let Some(why) = cyclops_proto::label::refusal(label) {
        // Dead ends invite the next action: name the pane, then come back.
        eprintln!(
            "--agent needs a name a pane can answer to. {why} \
             cyclops status shows every pane and its label; name one, then \
             rerun cyclops hooks install with that name."
        );
        return EXIT_USAGE;
    }
    let home = cyclops_proto::cyclops_home();
    let dest_dir = dest
        .map(Path::to_path_buf)
        .unwrap_or_else(|| hooks_root_in(&home).join(kind.name()).join(label));
    if inside_vendor_dir(&dest_dir) {
        eprintln!(
            "{} is a vendor config directory; cyclops prepares files and prints \
             instructions, it never writes vendor config itself. Use a neutral \
             --dest (default: $CYCLOPS_HOME/hooks/<vendor>/{label}/) and copy the file \
             yourself.",
            dest_dir.display()
        );
        return EXIT_USAGE;
    }
    let bin = cyclops_bin();
    let content = render(kind, label, &bin);
    let path = dest_dir.join(kind.file_name());
    if json {
        println!(
            "{}",
            json!({
                "cli": kind.name(),
                "agent": label,
                "path": path.display().to_string(),
                "written": !dry_run,
                "content": content,
            })
        );
    }
    if dry_run {
        if !json {
            println!("Would write {}:", path.display());
            println!();
            print!("{content}");
            println!();
            println!("{}", instructions(kind, &path, label));
        }
        return 0;
    }
    let receipt_problem = match write_artifact(&home, &dest_dir, kind, label, &bin, &content) {
        Ok(problem) => problem,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if let Some(e) = receipt_problem {
        eprintln!(
            "the hook config is written and usable, but its receipt ({}) is not: {e}. \
             cyclops start will leave this file alone instead of refreshing it when \
             the cyclops path changes; rerun this command to record one.",
            Receipt::path(&dest_dir).display()
        );
    }
    if !json {
        println!("Wrote {}", path.display());
        // Refresh walks only the default tree, so an explicit --dest
        // elsewhere stays the operator's to keep current.
        if dest.is_some() && !dest_dir.starts_with(hooks_root()) {
            println!(
                "  This dest is outside {}, so cyclops start will not refresh it when the \
                 cyclops path changes. Rerun this command after an update.",
                hooks_root().display()
            );
        }
        println!();
        println!("{}", instructions(kind, &path, label));
    }
    0
}

/// Render `kind`'s hook config for `label` in the standard place and return
/// its path, saying nothing on the way.
///
/// Write one rendered artifact and the receipt that makes it refreshable.
/// Both writers go through here: `run_install` for the verb and [`prepare`]
/// for launch wiring. What lands on disk cannot drift between them.
///
/// A failed receipt comes back as `Ok(Some(reason))`, not `Err`: the
/// artifact is written and correct, it is only unrefreshable later, and
/// saying "install failed" over a file that works would send the operator
/// after the wrong thing. Callers decide whether that note is worth a
/// sentence.
fn write_artifact(
    home: &Path,
    dest_dir: &Path,
    kind: CliKind,
    label: &str,
    bin: &str,
    content: &str,
) -> Result<Option<String>, String> {
    if let Ok(descendant_dir) = dest_dir.strip_prefix(home) {
        let root = StateRoot::open_or_create(home)
            .map_err(|e| format!("can't open {}: {e}", home.display()))?;
        let artifact = descendant_dir.join(kind.file_name());
        root.replace_file(&artifact, content.as_bytes())
            .map_err(|e| {
                format!(
                    "can't write {}: {e}",
                    dest_dir.join(kind.file_name()).display()
                )
            })?;
        let receipt = Receipt {
            vendor: kind.name().to_string(),
            agent: label.to_string(),
            file: kind.file_name().to_string(),
            bin: bin.to_string(),
            rendered_fnv: fnv64(content.as_bytes()),
        };
        return Ok(receipt.write_owned(&root, descendant_dir).err());
    }

    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("can't create {}: {e}", dest_dir.display()))?;
    let path = dest_dir.join(kind.file_name());
    write_atomic(&path, content).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    let receipt = Receipt {
        vendor: kind.name().to_string(),
        agent: label.to_string(),
        file: kind.file_name().to_string(),
        bin: bin.to_string(),
        rendered_fnv: fnv64(content.as_bytes()),
    };
    Ok(receipt.write(dest_dir).err().map(|e| e.to_string()))
}

/// This is `run_install`'s default-destination path with the printing and
/// the `--dest` handling taken off, for callers that need the artifact
/// rather than the report. `cyclops start` uses it to hand a pane its own
/// hook config at launch. It isolates a Cyclops-created Claude pane from the
/// user's default settings; direct Claude launches use the default settings
/// file wired by `start --setup-only --wire-hooks`.
///
/// A failed receipt is not worth even a note here: callers that want the
/// artifact want it either way, and a start that refused to launch a pane
/// over an unwritten receipt would be trading a working agent for a
/// bookkeeping line.
pub fn prepare(home: &Path, kind: CliKind, label: &str) -> Result<PathBuf, String> {
    let dest_dir = hooks_root_in(home).join(kind.name()).join(label);
    let bin = cyclops_bin();
    let content = render(kind, label, &bin);
    write_artifact(home, &dest_dir, kind, label, &bin, &content)?;
    Ok(dest_dir.join(kind.file_name()))
}

/// What one [`refresh`] run did, one classification per (vendor, label)
/// directory under `<home>/hooks/`.
#[derive(Default)]
pub struct Refreshed {
    /// The tree that was walked. Carried rather than recomputed, so a note
    /// names the directory this run actually touched.
    pub root: PathBuf,
    /// Artifacts rewritten for this build's path or templates.
    pub rewritten: Vec<PathBuf>,
    /// Artifacts already matching what this build renders. Counted and not
    /// named: a line on every run is noise.
    pub current: usize,
    /// Bytes that no longer hash to their receipt. The operator changed
    /// them, so they are never touched.
    pub edited: Vec<PathBuf>,
    /// Artifacts with no usable receipt, which is everything prepared by a
    /// build that predates them. Never touched.
    pub unmanaged: Vec<PathBuf>,
    /// The binary path the rewritten artifacts used to name, when this
    /// build moved. First one wins; a home holding two is a case nobody
    /// has and the note reads the same either way.
    pub moved_from: Option<String>,
    /// User-level vendor files still holding `moved_from`. Read-only
    /// evidence that a copy the operator already merged is broken.
    pub wired: Vec<PathBuf>,
    /// Artifacts naming another cyclops that is still on disk, with that
    /// path. Left alone: see [`refresh_one`].
    pub other_build: Vec<(PathBuf, String)>,
    /// Why a directory could not be refreshed, one sentence each.
    pub problems: Vec<String>,
}

/// Rewrite the prepared hook artifacts this build outdated, and nothing
/// else.
///
/// Called from `prepare_home`, so every `cyclops start`, every
/// `start --setup-only` and therefore every install and every update
/// converges. No timer is armed and nothing repeats (invariant 9): this
/// runs once, inside a command the operator typed.
///
/// The rule, and it is the whole design: an artifact is rewritten only
/// when the receipt beside it says Cyclops wrote it AND the bytes still
/// hash to what that receipt recorded. Anything else is reported once and
/// left exactly as it is. Nothing outside `<home>/hooks/` is written.
///
/// What this does NOT do: repair a copy the operator already merged into
/// vendor config. Cyclops never wrote that file and does not know where it
/// went, so a prefix move names it instead ([`wired_copies_holding`]).
pub fn refresh(home: &Path) -> Refreshed {
    let mut out = Refreshed {
        root: home.join("hooks"),
        ..Refreshed::default()
    };
    let root = match StateRoot::open_existing(home) {
        Ok(Some(root)) => root,
        Ok(None) => return out,
        Err(e) => {
            out.problems
                .push(format!("refresh {}: {e}", out.root.display()));
            return out;
        }
    };
    let bin = cyclops_bin();
    for vendor_dir in sorted_dirs(&out.root) {
        let Some(kind) = vendor_dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(CliKind::from_name)
        else {
            continue;
        };
        for label_dir in sorted_dirs(&vendor_dir) {
            let Some(label) = label_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // The daemon's own naming rule, from the same place install
            // used to refuse the name. A directory it rejects cannot be a
            // label any hook reports as.
            if cyclops_proto::label::refusal(label).is_some() {
                continue;
            }
            let descendant_dir = Path::new("hooks").join(kind.name()).join(label);
            refresh_one(
                &root,
                kind,
                label,
                &descendant_dir,
                &label_dir,
                &bin,
                &mut out,
            );
        }
    }
    if let Some(old) = out.moved_from.clone() {
        out.wired = wired_copies_holding(&wired_candidates(), &old);
    }
    out
}

/// One (vendor, label) directory, classified into exactly one of the four
/// outcomes and acted on.
fn refresh_one(
    root: &StateRoot,
    kind: CliKind,
    label: &str,
    descendant_dir: &Path,
    dir: &Path,
    bin: &str,
    out: &mut Refreshed,
) {
    let artifact_descendant = descendant_dir.join(kind.file_name());
    let artifact = dir.join(kind.file_name());
    // A receipt with no artifact beside it is the crash between the two
    // writes. There is nothing on disk to refresh.
    let bytes = match read_owned(root, &artifact_descendant) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return,
        Err(e) => {
            out.problems.push(e);
            return;
        }
    };
    // A receipt describing some other file is not proof about this one.
    let receipt = match Receipt::read_owned(root, descendant_dir) {
        Ok(Some(r))
            if r.vendor == kind.name() && r.agent == label && r.file == kind.file_name() =>
        {
            r
        }
        Err(e) => {
            out.problems.push(e);
            return;
        }
        _ => {
            out.unmanaged.push(artifact);
            return;
        }
    };
    if fnv64(&bytes) != receipt.rendered_fnv {
        out.edited.push(artifact);
        return;
    }
    let rendered = render(kind, label, bin);
    if rendered.as_bytes() == bytes.as_slice() {
        out.current += 1;
        return;
    }
    // The recorded binary still runs, so this is two builds on one machine
    // and not a prefix move. A developer running ./target/release/cyclops
    // would otherwise repoint every artifact at a path cargo clean
    // deletes, and the installed build would repoint them back.
    if receipt.bin != bin && Path::new(&receipt.bin).exists() {
        out.other_build.push((artifact, receipt.bin));
        return;
    }
    if let Err(e) = root.replace_file(&artifact_descendant, rendered.as_bytes()) {
        out.problems
            .push(format!("refresh {}: {e}", artifact.display()));
        return;
    }
    if receipt.bin != bin && out.moved_from.is_none() {
        out.moved_from = Some(receipt.bin);
    }
    let next = Receipt {
        vendor: kind.name().to_string(),
        agent: label.to_string(),
        file: kind.file_name().to_string(),
        bin: bin.to_string(),
        rendered_fnv: fnv64(rendered.as_bytes()),
    };
    if let Err(e) = next.write_owned(root, descendant_dir) {
        out.problems
            .push(format!("receipt {}: {e}", Receipt::path(dir).display()));
    }
    out.rewritten.push(artifact);
}

/// Immediate subdirectories of `parent`, sorted, so two runs over the same
/// home report in the same order.
fn sorted_dirs(parent: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = match fs::read_dir(parent) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => Vec::new(),
    };
    dirs.sort();
    dirs
}

/// The user-level files [`instructions`] tells the operator to merge a
/// prepared artifact into.
///
/// Project-local `<workspace>/.agents/hooks.json` and
/// `<workspace>/.cursor/hooks.json` are deliberately absent: nothing
/// records which workspaces exist, and listing a path Cyclops cannot
/// enumerate would imply coverage there is none.
fn wired_candidates() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let codex = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    vec![
        codex.join("hooks.json"),
        home.join(".cursor").join("hooks.json"),
        home.join(".claude").join("settings.json"),
    ]
}

/// Which of `candidates` still contain `old_bin`, by plain substring.
///
/// This module refuses to WRITE vendor config, and that is not what this
/// is. Reading back one literal string Cyclops itself baked into a file
/// the operator copied is what turns "every hook fails and the vendor
/// swallows it" into a named file and a fix. The bytes are never parsed,
/// never echoed, and never written.
fn wired_copies_holding(candidates: &[PathBuf], old_bin: &str) -> Vec<PathBuf> {
    if old_bin.is_empty() {
        return Vec::new();
    }
    candidates
        .iter()
        .filter(|p| fs::read_to_string(p).is_ok_and(|text| text.contains(old_bin)))
        .cloned()
        .collect()
}

/// The vendor and label a prepared artifact's path encodes, which is where
/// [`refresh`] read them from in the first place.
fn vendor_and_label(artifact: &Path) -> Option<(String, String)> {
    let dir = artifact.parent()?;
    let label = dir.file_name()?.to_str()?.to_string();
    let vendor = dir.parent()?.file_name()?.to_str()?.to_string();
    Some((vendor, label))
}

fn paths(list: &[PathBuf]) -> String {
    list.iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

impl Refreshed {
    /// The lines `prepare_home` prints under its ready line.
    ///
    /// Empty when nothing happened. Every entry here is something that
    /// changed or something the operator has to act on; a note repeated
    /// every run trains the reader to skip the run that mattered.
    pub fn notes(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if !self.rewritten.is_empty() {
            out.push(refreshed_note(self.rewritten.len(), &self.root));
        }
        if let Some(old) = &self.moved_from {
            out.push(moved_note(old, &self.wired, &self.root));
        }
        if !self.edited.is_empty() {
            out.push(edited_note(&self.edited));
        }
        if !self.unmanaged.is_empty() {
            out.push(unmanaged_note(&self.unmanaged));
        }
        for (artifact, bin) in &self.other_build {
            out.push(other_build_note(artifact, bin));
        }
        out.extend(self.problems.iter().cloned());
        out
    }
}

fn refreshed_note(n: usize, root: &Path) -> String {
    let thing = if n == 1 { "config" } else { "configs" };
    format!("refreshed {n} prepared hook {thing} in {}", root.display())
}

/// Said once after a prefix move. The prepared artifacts are fixed; a copy
/// already merged into vendor config is not, and cannot be without the
/// operator, so it is named rather than quietly claimed as repaired.
fn moved_note(old_bin: &str, wired: &[PathBuf], root: &Path) -> String {
    let held = if wired.is_empty() {
        "Nothing under $HOME still names it, but a copy you merged into a project's \
         .agents/ or .cursor/ is not checked."
            .to_string()
    } else {
        format!(
            "These still name it and their hooks fail silently until you change them: {}.",
            paths(wired)
        )
    };
    format!(
        "the prepared hook configs used to run {old_bin}; they now run this build. \
         {held} Replace the cyclops path in any wired copy, or recopy from {}.",
        root.display()
    )
}

fn edited_note(edited: &[PathBuf]) -> String {
    let thing = if edited.len() == 1 {
        "config"
    } else {
        "configs"
    };
    format!(
        "left {} edited prepared hook {thing} alone: {}. Delete one to have \
         cyclops hooks install prepare it fresh.",
        edited.len(),
        paths(edited)
    )
}

/// Everything prepared before receipts existed. Naming the one command
/// that brings a directory under management beats listing every path.
fn unmanaged_note(unmanaged: &[PathBuf]) -> String {
    let n = unmanaged.len();
    let (thing, has, which) = if n == 1 {
        ("config", "has", "it")
    } else {
        ("configs", "have", "one")
    };
    let cmd = vendor_and_label(&unmanaged[0])
        .map(|(vendor, label)| format!("cyclops hooks install {vendor} --agent {label}"))
        .unwrap_or_else(|| "cyclops hooks install".to_string());
    format!(
        "{n} prepared hook {thing} {has} no receipt, so cyclops leaves them alone \
         rather than guess they are its own; {cmd} brings {which} under refresh."
    )
}

fn other_build_note(artifact: &Path, bin: &str) -> String {
    format!(
        "{} runs {bin}, which is still on disk, so it was left alone. Two builds on \
         one machine: rerun cyclops hooks install from the build you want these \
         hooks to invoke.",
        artifact.display()
    )
}

pub fn run_verify(c: &mut Client, json: bool, style: &Style, target: &str) -> i32 {
    let result = match c.request("hooks.verify", json!({"target": target})) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, Some(target)));
            return 1;
        }
    };
    if json {
        println!("{result}");
        return i32::from(result["hooks_verified"] == false);
    }
    let sep = style.dim("·");
    let tier = result["tier"].as_u64().unwrap_or(2);
    let verified = result["hooks_verified"].as_bool();
    let events = result["events"].as_array().cloned().unwrap_or_default();
    // hooks_verified is absent on an unadopted pane even when its bound
    // manifest declares hooks (the events list below): edges are tracked
    // per label, so tracking starts at adoption. Only a pane with no
    // declared hooks at all reads "no hooks declared".
    let unadopted_with_hooks =
        verified.is_none() && result["manifest"].is_string() && !events.is_empty();
    let badge = match verified {
        Some(true) => "✔ hooks verified",
        Some(false) => "⚠ hooks unverified",
        None if unadopted_with_hooks => "hook tracking starts when the pane has a label",
        None => "no hooks declared",
    };
    println!(
        "{} {sep} tier {tier} {sep} {badge}",
        style.role(target, target)
    );
    if !events.is_empty() {
        println!();
        let name_w = events
            .iter()
            .filter_map(|e| e["event"].as_str())
            .map(display_width)
            .max()
            .unwrap_or(0);
        for e in &events {
            let name = e["event"].as_str().unwrap_or("?");
            let age = match e["last_seen_ms_ago"].as_u64() {
                Some(ms) if ms < 1000 => "just now".to_string(),
                Some(ms) => format!("{} ago", human_duration(ms)),
                None => "never".to_string(),
            };
            println!("  {}  {}", pad(name, name_w), style.dim(&age));
        }
    }
    match verified {
        Some(false) => {
            eprintln!(
                "No hook edge has ever reached the daemon from {target}. \
                 Run cyclops hooks selftest {target} to probe it; \
                 cyclops hooks install prints the wiring."
            );
            1
        }
        None if unadopted_with_hooks => {
            eprintln!(
                "{target} declares hooks but has no label, and hook edges \
                 only count for a labeled pane. Name the pane (cyclops \
                 status shows every pane and its label), then rerun \
                 cyclops hooks verify."
            );
            0
        }
        _ => 0,
    }
}

pub fn run_selftest(c: &mut Client, json: bool, style: &Style, target: &str) -> i32 {
    // The daemon delivers, then waits up to its own cap for resolution.
    c.set_read_timeout(SELFTEST_READ_TIMEOUT);
    let result = match c.request("hooks.selftest", json!({"target": target})) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", copy::client_error(&e, Some(target)));
            return 1;
        }
    };
    if json {
        println!("{result}");
        return i32::from(result["hook_ack"] != true);
    }
    let sep = style.dim("·");
    let hook_ack = result["hook_ack"] == true;
    let tier = result["tier"].as_u64().unwrap_or(2);
    // The delivery state renders in the same badge voice as a send
    // receipt; raw wire spellings never face a human.
    let state = match serde_json::from_value::<DeliveryState>(result["state"].clone()) {
        Ok(s) => receipt_badge(
            &DeliveryReceipt {
                to: target.to_string(),
                state: s,
                notification_state: None,
                quota_state: None,
                notification_settlement: None,
                wake_block: None,
                position: None,
                note: None,
                pane: None,
                held_by: None,
            },
            style,
        ),
        Err(_) => result["state"].as_str().unwrap_or("?").to_string(),
    };
    let msg_id = result["msg_id"].as_str().unwrap_or("?");
    let head = if hook_ack {
        "✔ ack hook fired with the marker"
    } else {
        "⚠ no hook ack"
    };
    println!(
        "{} {sep} {head} {sep} {state} {sep} {}",
        style.role(target, target),
        style.dim(msg_id)
    );
    if !hook_ack {
        if tier == 2 {
            eprintln!(
                "{target} has no payload-matchable ack hook (screen tier); \
                 a hook ack can never confirm it. The delivery state above is \
                 the whole answer."
            );
        } else {
            // Install takes a CLI kind, not the target: the daemon names
            // the bound manifest so this command runs as printed.
            eprintln!(
                "The ack hook never reported the marker. Its config is probably \
                 not loaded; cyclops hooks install {} --agent {} prints the \
                 wiring and the trust caveats.",
                result["manifest"].as_str().unwrap_or("<cli>"),
                result["target"].as_str().unwrap_or(target)
            );
        }
    }
    i32::from(!hook_ack)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_LABEL: &str = "reviewer";
    const GOLDEN_BIN: &str = "/opt/cyclops/bin/cyclops";

    fn golden(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// A shared config is read by every pane of one vendor, so a label
    /// inside it would make all of them report as the same agent. It must
    /// carry no --agent at all, and must still be the same events as the
    /// per-pane form: only the identity differs between the two.
    #[test]
    fn a_shared_config_names_no_agent_and_keeps_every_event() {
        for kind in [CliKind::Codex, CliKind::Agy, CliKind::Cursor] {
            let shared = render_shared(kind, GOLDEN_BIN);
            assert!(
                !shared.contains("--agent"),
                "{} shared config still names an agent:\n{shared}",
                kind.name()
            );
            let shared_v: serde_json::Value = serde_json::from_str(&shared)
                .unwrap_or_else(|e| panic!("{} shared config is not JSON: {e}", kind.name()));
            let pane_v: serde_json::Value =
                serde_json::from_str(&render(kind, GOLDEN_LABEL, GOLDEN_BIN)).unwrap();
            // Same event names on both sides, whatever the vendor's shape.
            fn keys(v: &serde_json::Value, into: &mut Vec<String>) {
                if let Some(o) = v.as_object() {
                    for (k, sv) in o {
                        into.push(k.clone());
                        keys(sv, into);
                    }
                }
            }
            let (mut a, mut b) = (Vec::new(), Vec::new());
            keys(&shared_v, &mut a);
            keys(&pane_v, &mut b);
            // agy keys its set by name, which is the one key that differs.
            a.retain(|k| k != SHARED_NAME);
            b.retain(|k| k != GOLDEN_LABEL);
            assert_eq!(a, b, "{} lost an event in the shared form", kind.name());
        }
    }

    /// The merge writes into a file this project does not own, so the two
    /// properties that matter are that the operator keeps everything they
    /// put there, and that running it twice is the same as running it once.
    #[test]
    fn the_merge_keeps_their_entries_and_is_idempotent() {
        let theirs = serde_json::json!({
            "hooks": {
                "Stop": [ { "hooks": [ { "type": "command", "command": "/bin/their-notifier" } ] } ],
                "SessionStart": [ { "hooks": [ { "type": "command", "command": "echo mine" } ] } ]
            },
            "unrelated": { "deeply": ["nested", "value"] }
        });
        let ours: serde_json::Value =
            serde_json::from_str(&render_shared(CliKind::Codex, GOLDEN_BIN)).unwrap();

        let mut doc = theirs.clone();
        merge_into(&mut doc, &ours);

        // Nothing of theirs was dropped or reordered ahead of itself.
        assert_eq!(doc["unrelated"], theirs["unrelated"]);
        assert_eq!(
            doc["hooks"]["SessionStart"],
            theirs["hooks"]["SessionStart"]
        );
        let stop = doc["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop[0], theirs["hooks"]["Stop"][0], "their handler moved");
        assert_eq!(stop.len(), 2, "ours should be appended, not replace theirs");

        // Twice is once. This is what keeps every update from growing a
        // duplicate handler in the operator's file.
        let once = doc.clone();
        merge_into(&mut doc, &ours);
        assert_eq!(doc, once, "a second merge changed the file");

        // And a stale entry from an older cyclops path is replaced rather
        // than accumulated beside the current one.
        let mut stale = theirs.clone();
        stale["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "hooks": [ { "type": "command", "command": "/old/path/cyclops hook Stop" } ]
            }));
        merge_into(&mut stale, &ours);
        let stop = stale["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "the stale cyclops entry should be gone");
        assert!(!stale.to_string().contains("/old/path/cyclops"));
    }

    #[test]
    fn wiring_bytes_can_be_checked_without_reopening_the_vendor_file() {
        let current = render_shared(CliKind::Claude, &cyclops_bin());
        assert!(matches!(
            inspect_wiring_bytes(CliKind::Claude, current.as_bytes()),
            WiringState::Current
        ));
        assert!(matches!(
            inspect_wiring_bytes(CliKind::Claude, b"{}"),
            WiringState::NeedsUpdate
        ));
        assert!(matches!(
            inspect_wiring_bytes(CliKind::Claude, b"not json"),
            WiringState::Invalid
        ));
    }

    /// Claude's normal direct launch reads its default settings file. An
    /// explicit --settings launch remains supported, but must not be the
    /// only route to lifecycle hooks.
    #[test]
    fn every_supported_vendor_has_a_default_hook_file() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        for (kind, file) in [
            (CliKind::Claude, "settings.json"),
            (CliKind::Codex, "hooks.json"),
            (CliKind::Agy, "hooks.json"),
            (CliKind::Cursor, "hooks.json"),
        ] {
            let p = vendor_hook_file(kind).expect("has a discovered path");
            assert_eq!(p.file_name().unwrap(), file);
            // CODEX_HOME can point codex's outside HOME, so it is exempt.
            if kind != CliKind::Codex || std::env::var_os("CODEX_HOME").is_none() {
                assert!(
                    p.starts_with(&home),
                    "{} escaped HOME: {}",
                    kind.name(),
                    p.display()
                );
            }
        }
    }

    #[test]
    fn rendered_templates_match_the_golden_files() {
        for (kind, file) in [
            (CliKind::Claude, "claude.settings.json"),
            (CliKind::Codex, "codex.hooks.json"),
            (CliKind::Agy, "agy.hooks.json"),
            (CliKind::Cursor, "cursor.hooks.json"),
        ] {
            assert_eq!(
                render(kind, GOLDEN_LABEL, GOLDEN_BIN),
                golden(file),
                "{file} drifted from the template; update hooks/ and the golden together"
            );
        }
    }

    #[test]
    fn rendered_templates_are_valid_vendor_json() {
        for kind in [
            CliKind::Claude,
            CliKind::Codex,
            CliKind::Agy,
            CliKind::Cursor,
        ] {
            let out = render(kind, GOLDEN_LABEL, GOLDEN_BIN);
            let v: serde_json::Value = serde_json::from_str(&out)
                .unwrap_or_else(|e| panic!("{} render is not JSON: {e}", kind.name()));
            // No placeholder survives rendering.
            assert!(!out.contains("{label}") && !out.contains("{cyclops_bin}"));
            // Every registered command is self-tagging. Claude omits the
            // mutable display label and relies on authenticated peer identity.
            let text = v.to_string();
            if kind == CliKind::Claude {
                assert!(!text.contains("--agent"), "Claude config embeds a label");
            } else {
                assert!(
                    text.contains(&format!("--agent {GOLDEN_LABEL}")),
                    "{}: command lost the agent",
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn claude_and_codex_share_the_measured_hook_shape() {
        for kind in [CliKind::Claude, CliKind::Codex] {
            let v: serde_json::Value = serde_json::from_str(&render(kind, "r", "cyclops")).unwrap();
            let hooks = v["hooks"].as_object().expect("hooks object");
            for (event, entries) in hooks {
                let cmd = entries[0]["hooks"][0]["command"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{}: {event} entry shape", kind.name()));
                let expected = if kind == CliKind::Claude {
                    format!("cyclops hook {event}")
                } else {
                    format!("cyclops hook {event} --agent r")
                };
                assert_eq!(cmd, expected);
            }
        }
        // Claude registers the four attention-relevant events.
        let v: serde_json::Value =
            serde_json::from_str(&render(CliKind::Claude, "r", "c")).unwrap();
        let mut events: Vec<&String> = v["hooks"].as_object().unwrap().keys().collect();
        events.sort();
        assert_eq!(
            events,
            [
                "Notification",
                "PermissionRequest",
                "Stop",
                "StopFailure",
                "UserPromptSubmit"
            ]
        );
    }

    #[test]
    fn agy_registers_every_event_with_distinct_commands() {
        // Payloads carry no event name, so every event needs its own
        // self-tagging command.
        let v: serde_json::Value = serde_json::from_str(&render(CliKind::Agy, "r", "c")).unwrap();
        let named = v["r"].as_object().expect("named hooks under the label");
        let mut events: Vec<&String> = named.keys().collect();
        events.sort();
        assert_eq!(
            events,
            [
                "PostInvocation",
                "PostToolUse",
                "PreInvocation",
                "PreToolUse",
                "Stop"
            ]
        );
        let mut commands: Vec<String> = Vec::new();
        for (event, entries) in named {
            let cmd = if entries[0]["command"].is_string() {
                entries[0]["command"].as_str().unwrap().to_string()
            } else {
                entries[0]["hooks"][0]["command"]
                    .as_str()
                    .unwrap()
                    .to_string()
            };
            assert!(cmd.contains(&format!("hook {event} ")), "{event}: {cmd}");
            commands.push(cmd);
        }
        commands.sort();
        commands.dedup();
        assert_eq!(commands.len(), 5, "commands must be distinct per event");
    }

    #[test]
    fn vendor_dot_dirs_are_refused() {
        for p in [
            "/Users/x/.claude/hooks",
            "/Users/x/.codex",
            "/home/x/.gemini/sub",
            "/work/project/.agents",
            "/work/project/.cursor",
        ] {
            assert!(inside_vendor_dir(Path::new(p)), "{p}");
        }
        assert!(!inside_vendor_dir(Path::new("/Users/x/.cyclops/hooks/rev")));
        assert!(!inside_vendor_dir(Path::new("/work/agents/hooks")));
    }

    #[test]
    fn a_linked_owned_hooks_directory_is_refused_without_touching_its_target() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let home = cyclops_proto::scratch::scratch_dir("hook-link-home");
        let external = cyclops_proto::scratch::scratch_dir("hook-link-external");
        for path in [&home, &external] {
            let _ = fs::remove_dir_all(path);
            fs::create_dir_all(path).unwrap();
        }
        let label_dir = external.join("codex").join(GOLDEN_LABEL);
        fs::create_dir_all(&label_dir).unwrap();
        let target = label_dir.join("hooks.json");
        fs::write(&target, b"external\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&external, home.join("hooks")).unwrap();

        let error = prepare(&home, CliKind::Codex, GOLDEN_LABEL).unwrap_err();

        assert!(
            error.contains("linked or non-directory component"),
            "{error}"
        );
        assert_eq!(fs::read(&target).unwrap(), b"external\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&external);
    }

    #[test]
    fn prepared_owned_hooks_and_receipts_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let home = cyclops_proto::scratch::scratch_dir("hook-owned-modes");
        let _ = fs::remove_dir_all(&home);
        let artifact = prepare(&home, CliKind::Codex, GOLDEN_LABEL).unwrap();
        let receipt = Receipt::path(artifact.parent().unwrap());

        for path in [&artifact, &receipt] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(artifact.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// A home with one prepared codex artifact, rendered for `bin` and
    /// vouched for by a matching receipt. Returns the home and the
    /// artifact path.
    fn prepared(tag: &str, bin: &str) -> (PathBuf, PathBuf) {
        let home = cyclops_proto::scratch::scratch_dir(&format!("hookref-{tag}"));
        let _ = fs::remove_dir_all(&home);
        let dir = home.join("hooks").join("codex").join(GOLDEN_LABEL);
        fs::create_dir_all(&dir).expect("create prepared dir");
        let body = render(CliKind::Codex, GOLDEN_LABEL, bin);
        let artifact = dir.join("hooks.json");
        fs::write(&artifact, &body).expect("write artifact");
        Receipt {
            vendor: "codex".to_string(),
            agent: GOLDEN_LABEL.to_string(),
            file: "hooks.json".to_string(),
            bin: bin.to_string(),
            rendered_fnv: fnv64(body.as_bytes()),
        }
        .write(&dir)
        .expect("write receipt");
        (home, artifact)
    }

    #[test]
    fn a_receipt_round_trips_through_its_own_file() {
        let dir = cyclops_proto::scratch::scratch_dir("hookrcpt");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create dir");
        Receipt {
            vendor: "codex".to_string(),
            agent: GOLDEN_LABEL.to_string(),
            file: "hooks.json".to_string(),
            bin: GOLDEN_BIN.to_string(),
            rendered_fnv: fnv64(b"body"),
        }
        .write(&dir)
        .expect("write receipt");

        let got = Receipt::read(&dir).expect("read back");
        assert_eq!(got.vendor, "codex");
        assert_eq!(got.agent, GOLDEN_LABEL);
        assert_eq!(got.file, "hooks.json");
        assert_eq!(got.bin, GOLDEN_BIN);
        assert_eq!(got.rendered_fnv, fnv64(b"body"));
        // A truncated or hand-mangled receipt is no proof at all, and
        // reads the same as no receipt.
        fs::write(Receipt::path(&dir), "{\"vendor\":\"codex\"}").unwrap();
        assert!(Receipt::read(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_artifact_rendered_for_this_build_is_current() {
        let (home, artifact) = prepared("cur", &cyclops_bin());
        let before = fs::read_to_string(&artifact).unwrap();

        let got = refresh(&home);
        assert_eq!(got.current, 1);
        assert!(got.rewritten.is_empty());
        assert!(got.edited.is_empty());
        assert!(got.unmanaged.is_empty());
        assert_eq!(fs::read_to_string(&artifact).unwrap(), before);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_hand_edited_artifact_is_never_rewritten() {
        // The receipt vouches for the rendered bytes; the file on disk has
        // one more event the operator added. That is a measurement, and it
        // outranks anything this build would render.
        let (home, artifact) = prepared("edit", &cyclops_bin());
        let mine = format!("{}\n", fs::read_to_string(&artifact).unwrap().trim_end());
        fs::write(&artifact, format!("{mine}// mine\n")).unwrap();
        let before = fs::read_to_string(&artifact).unwrap();

        let got = refresh(&home);
        assert_eq!(got.edited, vec![artifact.clone()]);
        assert!(got.rewritten.is_empty());
        assert_eq!(fs::read_to_string(&artifact).unwrap(), before);
        assert!(got.notes().iter().any(|n| n.contains("left 1 edited")));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_prefix_move_repoints_the_artifact_and_names_the_old_path() {
        // The recorded binary is gone, which is exactly what a moved
        // install prefix looks like from here.
        let old = "/nonexistent/old-prefix/cyclops";
        let (home, artifact) = prepared("moved", old);

        let got = refresh(&home);
        assert_eq!(got.rewritten, vec![artifact.clone()]);
        assert_eq!(got.moved_from.as_deref(), Some(old));
        let body = fs::read_to_string(&artifact).unwrap();
        assert!(!body.contains(old), "old path survived: {body}");
        assert!(body.contains(&cyclops_bin()), "{body}");
        // The receipt now vouches for the new bytes, so a second run is a
        // no-op rather than a rewrite loop.
        let again = refresh(&home);
        assert_eq!(again.current, 1);
        assert!(again.rewritten.is_empty());
        assert!(again.moved_from.is_none());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn an_artifact_naming_a_binary_that_still_exists_is_left_alone() {
        // Two builds on one machine. Repointing here would hand the
        // artifact to whichever build ran start last.
        let (home, artifact) = prepared("twobuilds", "/placeholder");
        let other = home.join("other-cyclops");
        fs::write(&other, b"#!/bin/sh\n").unwrap();
        let other_bin = other.to_str().unwrap().to_string();
        let body = render(CliKind::Codex, GOLDEN_LABEL, &other_bin);
        fs::write(&artifact, &body).unwrap();
        let dir = artifact.parent().unwrap();
        Receipt {
            vendor: "codex".to_string(),
            agent: GOLDEN_LABEL.to_string(),
            file: "hooks.json".to_string(),
            bin: other_bin.clone(),
            rendered_fnv: fnv64(body.as_bytes()),
        }
        .write(dir)
        .unwrap();

        let got = refresh(&home);
        assert!(got.rewritten.is_empty());
        assert!(got.moved_from.is_none());
        assert_eq!(got.other_build, vec![(artifact.clone(), other_bin)]);
        assert_eq!(fs::read_to_string(&artifact).unwrap(), body);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn an_artifact_with_no_receipt_is_unmanaged_and_untouched() {
        // Every home prepared by a build that predates receipts.
        let (home, artifact) = prepared("unmanaged", "/nonexistent/old/cyclops");
        fs::remove_file(Receipt::path(artifact.parent().unwrap())).unwrap();
        let before = fs::read_to_string(&artifact).unwrap();

        let got = refresh(&home);
        assert_eq!(got.unmanaged, vec![artifact.clone()]);
        assert!(got.rewritten.is_empty());
        assert_eq!(fs::read_to_string(&artifact).unwrap(), before);
        assert!(got
            .notes()
            .iter()
            .any(|n| n.contains("cyclops hooks install codex --agent reviewer")));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn refresh_walks_only_vendor_directories_it_wrote() {
        let (home, _) = prepared("walk", &cyclops_bin());
        // A vendor name no CliKind answers to, and a label the daemon
        // would refuse. Both hold a plausible artifact and a receipt.
        for (vendor, label) in [("gemini", "reviewer"), ("codex", "admin")] {
            let dir = home.join("hooks").join(vendor).join(label);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("hooks.json"), "{}\n").unwrap();
        }

        let got = refresh(&home);
        assert_eq!(got.current, 1);
        assert!(got.unmanaged.is_empty(), "{:?}", got.unmanaged);
        assert!(got.edited.is_empty());
        assert!(got.problems.is_empty());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_wired_search_reads_only_the_named_files_and_matches_the_literal() {
        let dir = cyclops_proto::scratch::scratch_dir("hookwired");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let hit = dir.join("hooks.json");
        let miss = dir.join("settings.json");
        let absent = dir.join("never-written.json");
        fs::write(&hit, "{\"command\":\"/old/bin/cyclops hook Stop\"}").unwrap();
        fs::write(&miss, "{\"command\":\"/new/bin/cyclops hook Stop\"}").unwrap();

        let candidates = vec![hit.clone(), miss, absent];
        assert_eq!(
            wired_copies_holding(&candidates, "/old/bin/cyclops"),
            vec![hit]
        );
        // An empty old path would match every file; it never searches.
        assert!(wired_copies_holding(&candidates, "").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_registers_only_the_two_measured_events() {
        // The daemon matches incoming events against the manifest's own
        // ack/turn_start/turn_end names (both beforeSubmitPrompt here), so
        // wiring any other Cursor event would just be ignored; this proves
        // the template does not do so, and that the flat schema (no nested
        // "hooks" array, no "type": "command") round-trips correctly.
        let v: serde_json::Value =
            serde_json::from_str(&render(CliKind::Cursor, "r", "cyclops")).unwrap();
        assert_eq!(v["version"], 1);
        let hooks = v["hooks"].as_object().expect("hooks object");
        let mut events: Vec<&String> = hooks.keys().collect();
        events.sort();
        assert_eq!(events, ["beforeSubmitPrompt", "stop"]);
        for (event, entries) in hooks {
            let cmd = entries[0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("cursor: {event} entry shape"));
            assert_eq!(cmd, format!("cyclops hook {event} --agent r"));
        }
    }
}
