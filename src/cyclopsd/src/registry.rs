//! The adoption registry: which pane wears which cyclops label.
//!
//! Adoption is explicit (v1 keeper). `cyclops name <target> <label>` puts a
//! pane in here; nothing else does. The label is what names a sender, what
//! resolves a recipient, and what the `*` broadcast means, so this map is
//! the roster the whole product runs on.
//!
//! M1 kept it in memory and lost it on every daemon restart, which meant a
//! crash silently unnamed every agent and every queued message lost its
//! recipient. It is now a file under `$CYCLOPS_HOME`, written whole on
//! every change and read back at boot.
//!
//! Two things ride along with the label because they belong to the same
//! fact and would otherwise need a second file:
//!
//! - The explicit manifest pin from `--manifest`. Absent means autodetect,
//!   which is what every pane did before this existed.
//! - The tmux chrome this pane and its window wore BEFORE cyclops touched
//!   them, so `--clear` and daemon shutdown can put them back. The
//!   snapshot is taken once, at adoption, and never re-taken: a daemon
//!   that crashed and came back would otherwise snapshot its own chrome
//!   and "restore" cyclops's border format forever.
//!
//! ## Restoring across a restart
//!
//! A tmux pane id is unique for the life of a tmux SERVER and starts over
//! at %0 when that server restarts. Replaying the file blindly would
//! therefore hand an old label to whatever pane inherited the id. Restore
//! keeps an entry only when the pane still exists AND its root process is
//! the same pid it was at adoption; everything else is pruned. The pid is
//! the same occupant identity the delivery gate re-checks before it pastes.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name under `$CYCLOPS_HOME`.
const FILE: &str = "registry.json";

/// Bumped only when the shape changes incompatibly. A file from the future
/// is ignored rather than misread.
const VERSION: u32 = 1;

/// One adopted pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Adoption {
    /// Watched session the pane belongs to. Carried so a ledger line can
    /// be written for the right session without re-resolving the pane.
    pub(crate) session: String,
    pub(crate) pane_id: String,
    pub(crate) label: String,
    /// Explicit manifest pin from `--manifest`. None means autodetect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) manifest: Option<String>,
    /// Root process of the pane at adoption; the occupant identity restore
    /// checks against.
    pub(crate) pane_pid: i32,
    /// Window this pane sat in at adoption, for the window-scoped half of
    /// the chrome restore.
    pub(crate) window_id: String,
    /// `pane-border-format` as set AT PANE SCOPE when cyclops arrived.
    /// None means it was unset there and restore unsets it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) border_format: Option<String>,
}

/// The window-scoped half of the chrome snapshot.
///
/// `pane-border-status` decides whether a border carries text at all, and
/// tmux has no pane scope for it (F27): `set -p` on it writes the window
/// option. So it is snapshotted per window, turned on by the first
/// adoption in that window, and put back when the last one leaves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WindowChrome {
    pub(crate) session: String,
    pub(crate) window_id: String,
    /// `pane-border-status` as set AT WINDOW SCOPE when cyclops arrived.
    /// None means it was unset there and restore unsets it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) border_status: Option<String>,
}

/// On-disk shape. Vectors rather than maps so the file reads in a sensible
/// order and jq can walk it like the ledger.
#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    panes: Vec<Adoption>,
    #[serde(default)]
    windows: Vec<WindowChrome>,
}

/// The live registry. Keyed by tmux id, which is unique per tmux server;
/// the session name rides inside each entry rather than keying it.
pub(crate) struct Registry {
    path: PathBuf,
    panes: HashMap<String, Adoption>,
    windows: HashMap<String, WindowChrome>,
}

impl Registry {
    /// Read `<home>/registry.json`. A missing file is an empty registry.
    /// An unreadable or unparseable one is a warning and an empty
    /// registry: a broken cache must not stop the daemon from booting,
    /// and the ledger still holds every adoption that ever happened.
    pub(crate) fn load(home: &Path) -> (Registry, Vec<String>) {
        let path = home.join(FILE);
        let mut reg = Registry {
            path,
            panes: HashMap::new(),
            windows: HashMap::new(),
        };
        let shown = reg.path.display().to_string();
        let text = match std::fs::read_to_string(&reg.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (reg, Vec::new()),
            Err(e) => return (reg, vec![format!("cannot read {shown}: {e}")]),
        };
        let file: File = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                return (
                    reg,
                    vec![format!(
                        "{shown} is not readable as a registry ({e}); no pane starts adopted"
                    )],
                )
            }
        };
        if file.version != VERSION {
            return (
                reg,
                vec![format!(
                    "{shown} is version {} and this daemon reads {VERSION}; no pane starts adopted",
                    file.version
                )],
            );
        }
        for a in file.panes {
            reg.panes.insert(a.pane_id.clone(), a);
        }
        for w in file.windows {
            reg.windows.insert(w.window_id.clone(), w);
        }
        (reg, Vec::new())
    }

    /// Label of a pane, if adopted.
    pub(crate) fn label_of(&self, pane_id: &str) -> Option<String> {
        self.panes.get(pane_id).map(|a| a.label.clone())
    }

    /// Explicit manifest pin for a pane, if one was set.
    pub(crate) fn manifest_of(&self, pane_id: &str) -> Option<String> {
        self.panes.get(pane_id).and_then(|a| a.manifest.clone())
    }

    /// Pane a label points at.
    pub(crate) fn pane_for_label(&self, label: &str) -> Option<String> {
        self.panes
            .values()
            .find(|a| a.label == label)
            .map(|a| a.pane_id.clone())
    }

    /// True when another pane already wears this label. Labels are unique
    /// across every watched session: they are addresses.
    pub(crate) fn label_taken_by_other(&self, label: &str, pane_id: &str) -> bool {
        self.panes
            .values()
            .any(|a| a.label == label && a.pane_id != pane_id)
    }

    /// pane id -> label, for the surfaces that render the whole roster.
    pub(crate) fn labels(&self) -> HashMap<String, String> {
        self.panes
            .iter()
            .map(|(id, a)| (id.clone(), a.label.clone()))
            .collect()
    }

    pub(crate) fn get(&self, pane_id: &str) -> Option<&Adoption> {
        self.panes.get(pane_id)
    }

    /// Distinct session names with at least one adoption on file. Boot
    /// verifies the ones it will not watch against tmux (lib.rs); an entry
    /// nothing verifies would hold its label forever.
    pub(crate) fn sessions(&self) -> Vec<String> {
        let mut out: Vec<String> = self.panes.values().map(|a| a.session.clone()).collect();
        out.sort();
        out.dedup();
        out
    }

    /// Adoptions in a session, for chrome re-apply after a reattach.
    pub(crate) fn in_session(&self, session: &str) -> Vec<Adoption> {
        let mut out: Vec<Adoption> = self
            .panes
            .values()
            .filter(|a| a.session == session)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        out
    }

    /// Window chrome snapshot for a window, if cyclops took one.
    pub(crate) fn window(&self, window_id: &str) -> Option<&WindowChrome> {
        self.windows.get(window_id)
    }

    /// Add or replace an adoption, and the window snapshot when this is
    /// the first adoption in that window. Persists before it commits: an
    /// adoption that cannot be written down did not happen.
    pub(crate) fn adopt(
        &mut self,
        adoption: Adoption,
        window: WindowChrome,
    ) -> Result<(), std::io::Error> {
        let mut panes = self.panes.clone();
        let mut windows = self.windows.clone();
        windows
            .entry(window.window_id.clone())
            .or_insert_with(|| window);
        panes.insert(adoption.pane_id.clone(), adoption);
        self.commit(panes, windows)
    }

    /// What [`clear`](Self::clear) would hand back, without handing it back.
    ///
    /// The chrome restore has to run BEFORE the entry is forgotten, because
    /// that entry is the only copy of the border settings tmux had before
    /// cyclops: commit the removal first and a failed restore destroys them.
    /// So the caller looks with this, restores, and only then clears.
    pub(crate) fn pending_clear(&self, pane_id: &str) -> Option<(Adoption, Option<WindowChrome>)> {
        let gone = self.panes.get(pane_id)?.clone();
        // Same rule as the committed path, run on copies: `release_window`
        // counts an honest map, which means one with this pane already out.
        let mut panes = self.panes.clone();
        panes.remove(pane_id);
        let mut windows = self.windows.clone();
        let freed = release_window(&panes, &mut windows, &gone.window_id);
        Some((gone, freed))
    }

    /// Un-adopt a pane. Returns the entry that was there, plus the window
    /// snapshot when this was the last adopted pane in that window.
    pub(crate) fn clear(
        &mut self,
        pane_id: &str,
    ) -> Result<Option<(Adoption, Option<WindowChrome>)>, std::io::Error> {
        let mut panes = self.panes.clone();
        let Some(gone) = panes.remove(pane_id) else {
            return Ok(None);
        };
        let mut windows = self.windows.clone();
        let freed = release_window(&panes, &mut windows, &gone.window_id);
        self.commit(panes, windows)?;
        Ok(Some((gone, freed)))
    }

    /// Move an adopted pane into another window.
    ///
    /// `destination_status` is the destination window's chrome as tmux
    /// reports it now, and is kept only when the registry has never seen
    /// that window; a window already holding an adopted pane already has
    /// its pre-cyclops snapshot recorded.
    ///
    /// Returns the source window's snapshot when the move left it with no
    /// adopted panes, which is the caller's signal to hand that window's
    /// border back.
    pub(crate) fn move_window(
        &mut self,
        pane_id: &str,
        destination: &str,
        destination_status: Option<String>,
    ) -> Result<Option<WindowChrome>, std::io::Error> {
        let mut panes = self.panes.clone();
        let Some(moved) = panes.get_mut(pane_id) else {
            return Ok(None);
        };
        if moved.window_id == destination {
            return Ok(None);
        }
        let session = moved.session.clone();
        let source = std::mem::replace(&mut moved.window_id, destination.to_string());
        let mut windows = self.windows.clone();
        windows
            .entry(destination.to_string())
            .or_insert_with(|| WindowChrome {
                session,
                window_id: destination.to_string(),
                border_status: destination_status,
            });
        let freed = release_window(&panes, &mut windows, &source);
        self.commit(panes, windows)?;
        Ok(freed)
    }

    /// Drop entries the live pane table no longer justifies, and hand back
    /// the survivors. `live` is (pane id, root pid) for every pane the
    /// session currently has.
    ///
    /// Two reasons to drop, and both are the same reason: the pane cyclops
    /// adopted is not there any more. Either the id is gone, or the id is
    /// there with a different process behind it, which after a tmux server
    /// restart is a different pane entirely.
    pub(crate) fn restore_session(
        &mut self,
        session: &str,
        live: &[(String, i32)],
    ) -> Result<Vec<Adoption>, std::io::Error> {
        let mut panes = self.panes.clone();
        panes.retain(|_, a| {
            a.session != session
                || live
                    .iter()
                    .any(|(id, pid)| *id == a.pane_id && *pid == a.pane_pid)
        });
        let mut windows = self.windows.clone();
        windows.retain(|id, w| w.session != session || panes.values().any(|a| a.window_id == *id));
        let kept: Vec<Adoption> = {
            let mut v: Vec<Adoption> = panes
                .values()
                .filter(|a| a.session == session)
                .cloned()
                .collect();
            v.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
            v
        };
        self.commit(panes, windows)?;
        Ok(kept)
    }

    /// A pane went away. Adoption ends with the pane (M1 rule); the entry
    /// and any window snapshot it was the last holder of go with it.
    pub(crate) fn forget(&mut self, pane_id: &str) -> Option<(Adoption, Option<WindowChrome>)> {
        match self.clear(pane_id) {
            Ok(gone) => gone,
            Err(e) => {
                // The pane is gone either way; refusing to forget it would
                // leave a dead label answering to messages. A failed commit
                // left the live maps untouched, so drop it from them here
                // and hand the window back by the same rule.
                tracing::error!(pane = pane_id, error = %e, "registry write failed while forgetting a closed pane");
                let gone = self.panes.remove(pane_id)?;
                let freed = release_window(&self.panes, &mut self.windows, &gone.window_id);
                Some((gone, freed))
            }
        }
    }

    /// Write the candidate state, then take it. Order matters: a caller
    /// that saw Ok must be able to restart and find the same roster.
    fn commit(
        &mut self,
        panes: HashMap<String, Adoption>,
        windows: HashMap<String, WindowChrome>,
    ) -> Result<(), std::io::Error> {
        let mut file = File {
            version: VERSION,
            panes: panes.values().cloned().collect(),
            windows: windows.values().cloned().collect(),
        };
        file.panes.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        file.windows.sort_by(|a, b| a.window_id.cmp(&b.window_id));
        write_atomic(&self.path, &file)?;
        self.panes = panes;
        self.windows = windows;
        Ok(())
    }
}

/// Hand back a window's chrome snapshot when the pane that just left was
/// the last adopted one in it, and drop it from `windows` in the same
/// breath. None means some adopted pane is still there and the window
/// keeps its border text.
///
/// One rule, three ways to leave a window: `--clear`, a pane that closed,
/// and a pane that moved out. `panes` must already be the map with the
/// departing pane gone or moved, which is what makes the count honest.
fn release_window(
    panes: &HashMap<String, Adoption>,
    windows: &mut HashMap<String, WindowChrome>,
    window_id: &str,
) -> Option<WindowChrome> {
    if panes.values().any(|a| a.window_id == window_id) {
        None
    } else {
        windows.remove(window_id)
    }
}

/// Write the whole file through a temp file and one rename, owner-only.
/// A torn registry after a crash mid-write would unname a live agent.
fn write_atomic(path: &Path, file: &File) -> Result<(), std::io::Error> {
    let mut text = serde_json::to_string_pretty(file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    text.push('\n');
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-reg-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch home");
        dir
    }

    fn adoption(pane: &str, label: &str, pid: i32, window: &str) -> Adoption {
        Adoption {
            session: "main".into(),
            pane_id: pane.into(),
            label: label.into(),
            manifest: None,
            pane_pid: pid,
            window_id: window.into(),
            border_format: None,
        }
    }

    fn window(id: &str, status: Option<&str>) -> WindowChrome {
        WindowChrome {
            session: "main".into(),
            window_id: id.into(),
            border_status: status.map(String::from),
        }
    }

    #[test]
    fn an_adoption_survives_a_reload() {
        let dir = home("survives");
        let (mut reg, warnings) = Registry::load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut a = adoption("%1", "reviewer", 4242, "@0");
        a.manifest = Some("claude".into());
        a.border_format = Some("old-format".into());
        reg.adopt(a, window("@0", None)).expect("adopt writes");

        let (back, warnings) = Registry::load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back.label_of("%1").as_deref(), Some("reviewer"));
        assert_eq!(back.manifest_of("%1").as_deref(), Some("claude"));
        assert_eq!(back.pane_for_label("reviewer").as_deref(), Some("%1"));
        assert_eq!(
            back.get("%1").and_then(|a| a.border_format.clone()),
            Some("old-format".into())
        );
        assert!(back.window("@0").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole reason the pid is stored. After a tmux server restart the
    /// ids start over, so an entry whose pane id is back with a different
    /// process is a different pane and must not inherit the name.
    #[test]
    fn restore_keeps_the_same_occupant_and_drops_everything_else() {
        let dir = home("restore");
        let (mut reg, _) = Registry::load(&dir);
        reg.adopt(adoption("%1", "reviewer", 100, "@0"), window("@0", None))
            .unwrap();
        reg.adopt(adoption("%2", "implementer", 200, "@0"), window("@0", None))
            .unwrap();
        reg.adopt(adoption("%3", "tests", 300, "@1"), window("@1", None))
            .unwrap();

        // %1 same pane, %2 same id with a new process, %3 gone entirely.
        let kept = reg
            .restore_session("main", &[("%1".into(), 100), ("%2".into(), 999)])
            .expect("restore writes");
        assert_eq!(
            kept.iter().map(|a| a.label.as_str()).collect::<Vec<_>>(),
            vec!["reviewer"]
        );
        assert!(reg.label_of("%2").is_none());
        assert!(reg.label_of("%3").is_none());
        // @1 lost its only adopted pane, so its snapshot goes too.
        assert!(reg.window("@1").is_none());
        assert!(reg.window("@0").is_some());

        let (back, _) = Registry::load(&dir);
        assert_eq!(back.labels().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The window snapshot belongs to the window, not to whichever pane
    /// was adopted first: the second adoption must not overwrite it with
    /// the value cyclops itself just set, and clearing one of two must not
    /// hand the window back early.
    #[test]
    fn the_window_snapshot_is_taken_once_and_released_last() {
        let dir = home("window");
        let (mut reg, _) = Registry::load(&dir);
        reg.adopt(adoption("%1", "a", 1, "@0"), window("@0", None))
            .unwrap();
        reg.adopt(adoption("%2", "b", 2, "@0"), window("@0", Some("top")))
            .unwrap();
        assert!(
            reg.window("@0").unwrap().border_status.is_none(),
            "the first snapshot is the one that was there before cyclops"
        );

        let (_, freed) = reg.clear("%1").unwrap().expect("cleared");
        assert!(freed.is_none(), "%2 is still adopted in @0");
        assert!(reg.window("@0").is_some());

        let (_, freed) = reg.clear("%2").unwrap().expect("cleared");
        assert!(freed.is_some(), "the last one out restores the window");
        assert!(reg.window("@0").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Moving a pane is an un-adoption from one window and an adoption
    /// into another, and both halves have to be right: the destination
    /// keeps the snapshot it already had rather than one taken after
    /// cyclops wrote to it, and the source is only handed back once the
    /// last adopted pane has left it.
    #[test]
    fn moving_a_pane_frees_the_source_only_when_it_empties() {
        let dir = home("move");
        let (mut reg, _) = Registry::load(&dir);
        reg.adopt(adoption("%1", "a", 1, "@0"), window("@0", None))
            .unwrap();
        reg.adopt(adoption("%2", "b", 2, "@0"), window("@0", None))
            .unwrap();

        // %1 leaves @0 for a window cyclops has never seen.
        let freed = reg
            .move_window("%1", "@1", Some("bottom".into()))
            .expect("move writes");
        assert!(freed.is_none(), "%2 is still adopted in @0");
        assert_eq!(reg.get("%1").map(|a| a.window_id.as_str()), Some("@1"));
        assert_eq!(
            reg.window("@1").and_then(|w| w.border_status.as_deref()),
            Some("bottom"),
            "the destination's own prior setting is what gets restored later"
        );

        // %2 follows. A destination cyclops already painted must keep the
        // snapshot from the first arrival, not a reading of its own work.
        let freed = reg
            .move_window("%2", "@1", Some("top".into()))
            .expect("move writes");
        let freed = freed.expect("@0 is empty now");
        assert_eq!(freed.window_id, "@0");
        assert!(freed.border_status.is_none());
        assert_eq!(
            reg.window("@1").and_then(|w| w.border_status.as_deref()),
            Some("bottom")
        );
        assert!(reg.window("@0").is_none());

        // A move that goes nowhere changes nothing.
        assert!(reg.move_window("%2", "@1", None).unwrap().is_none());
        assert!(reg.move_window("%9", "@1", None).unwrap().is_none());

        let (back, _) = Registry::load(&dir);
        assert_eq!(back.get("%1").map(|a| a.window_id.as_str()), Some("@1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `forget` has a second path: the pane is gone, but the file could not
    /// be rewritten. It has to release the window by the same rule the
    /// committed path uses, or a window whose last named pane closed keeps
    /// border text nothing will take off. Nothing else reaches this path.
    #[test]
    fn forgetting_a_pane_releases_its_window_even_when_the_file_cannot_be_written() {
        let dir = home("forget");
        let (mut reg, _) = Registry::load(&dir);
        reg.adopt(adoption("%1", "a", 1, "@0"), window("@0", Some("bottom")))
            .unwrap();
        reg.adopt(adoption("%2", "b", 2, "@0"), window("@0", None))
            .unwrap();

        // A path no write can succeed on: the parent is a regular file, so
        // even the create_dir_all in front of the temp file fails.
        let blocked = dir.join("not-a-directory");
        std::fs::write(&blocked, "").unwrap();
        reg.path = blocked.join(FILE);

        let (gone, freed) = reg.forget("%1").expect("the pane is forgotten either way");
        assert_eq!(gone.label, "a");
        assert!(freed.is_none(), "%2 is still adopted in @0");

        let (gone, freed) = reg.forget("%2").expect("forgotten");
        assert_eq!(gone.label, "b");
        assert_eq!(
            freed
                .expect("the last one out releases the window")
                .border_status
                .as_deref(),
            Some("bottom")
        );
        assert!(reg.window("@0").is_none());
        assert!(reg.forget("%1").is_none(), "already gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_warns_and_starts_empty() {
        let dir = home("broken");
        std::fs::write(dir.join(FILE), "{not json").unwrap();
        let (reg, warnings) = Registry::load(&dir);
        assert!(reg.labels().is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        std::fs::write(dir.join(FILE), r#"{"version":99,"panes":[]}"#).unwrap();
        let (reg, warnings) = Registry::load(&dir);
        assert!(reg.labels().is_empty());
        assert!(warnings[0].contains("version 99"), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn labels_are_unique_across_sessions() {
        let dir = home("unique");
        let (mut reg, _) = Registry::load(&dir);
        reg.adopt(adoption("%1", "reviewer", 1, "@0"), window("@0", None))
            .unwrap();
        assert!(reg.label_taken_by_other("reviewer", "%2"));
        assert!(!reg.label_taken_by_other("reviewer", "%1"));
        assert!(!reg.label_taken_by_other("implementer", "%2"));
        assert_eq!(reg.sessions(), vec!["main"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
