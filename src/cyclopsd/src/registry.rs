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

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use cyclops_proto::{ProcessInstanceId, RecipientKey};
use cyclops_state::{StateError, StateRoot};
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
    /// Exact durable mailbox identity. Legacy rows without it are retained
    /// for chrome recovery but cannot address a mailbox until re-adopted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) recipient: Option<RecipientKey>,
    /// Pane-root generation captured with `recipient` at adoption time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pane_root: Option<ProcessInstanceId>,
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

/// The live registry. Exact adoptions are keyed by their durable mailbox
/// recipient. Legacy pane-id rows stay on disk for a later explicit rebind,
/// but never participate in addressing.
pub(crate) struct Registry {
    state_root: Arc<StateRoot>,
    panes: HashMap<RecipientKey, Adoption>,
    legacy_panes: Vec<Adoption>,
    windows: HashMap<String, WindowChrome>,
}

impl Registry {
    /// Read `<home>/registry.json`. A missing file is an empty registry.
    /// An unreadable or unparseable one is a warning and an empty
    /// registry: a broken cache must not stop the daemon from booting,
    /// and the ledger still holds every adoption that ever happened.
    pub(crate) fn load(state_root: Arc<StateRoot>) -> (Registry, Vec<String>) {
        let mut reg = Registry {
            state_root,
            panes: HashMap::new(),
            legacy_panes: Vec::new(),
            windows: HashMap::new(),
        };
        let path = reg.state_root.path().join(FILE);
        let shown = path.display().to_string();
        let mut source = match reg.state_root.open_read(Path::new(FILE)) {
            Ok(Some(source)) => source,
            Ok(None) => return (reg, Vec::new()),
            Err(error) => return (reg, vec![format!("cannot read {shown}: {error}")]),
        };
        let mut text = String::new();
        if let Err(error) = source.read_to_string(&mut text) {
            return (reg, vec![format!("cannot read {shown}: {error}")]);
        }
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
        for adoption in file.panes {
            match (adoption.recipient, adoption.pane_root) {
                (Some(recipient), Some(_)) => {
                    reg.panes.insert(recipient, adoption);
                }
                _ => reg.legacy_panes.push(adoption),
            }
        }
        for w in file.windows {
            reg.windows.insert(w.window_id.clone(), w);
        }
        (reg, Vec::new())
    }

    /// Label of a pane, if adopted.
    #[cfg(test)]
    pub(crate) fn label_of(&self, pane_id: &str) -> Option<String> {
        self.unique_for_pane(pane_id).map(|a| a.label.clone())
    }

    /// Explicit manifest pin for a pane, if one was set.
    #[cfg(test)]
    pub(crate) fn manifest_of(&self, pane_id: &str) -> Option<String> {
        self.unique_for_pane(pane_id)
            .and_then(|a| a.manifest.clone())
    }

    /// Adoption that still names this exact recipient and pane generation.
    pub(crate) fn for_route(
        &self,
        recipient: RecipientKey,
        pane_root: ProcessInstanceId,
    ) -> Option<&Adoption> {
        self.panes
            .get(&recipient)
            .filter(|adoption| adoption.pane_root == Some(pane_root))
    }

    /// Exact adoption for a durable recipient, regardless of pane root.
    pub(crate) fn for_recipient(&self, recipient: RecipientKey) -> Option<&Adoption> {
        self.panes.get(&recipient)
    }

    /// Unique adoption for one server-wide pane and root generation.
    ///
    /// A pane may move between sessions before both watchers observe the
    /// transfer. The old durable route remains authoritative for mailbox
    /// history, while this physical lookup carries only its manifest pin and
    /// recovery ownership across that observation gap.
    pub(crate) fn for_physical_pane(
        &self,
        pane_id: &str,
        pane_root: ProcessInstanceId,
    ) -> Option<&Adoption> {
        let mut matches = self.panes.values().filter(|adoption| {
            adoption.pane_id == pane_id && adoption.pane_root == Some(pane_root)
        });
        let found = matches.next()?;
        matches.next().is_none().then_some(found)
    }

    /// Every retained route for one server-wide pane id.
    ///
    /// Normally there is one. Returning all keeps physical-loss cleanup
    /// complete if a pane crossed routes before an older registry was repaired.
    pub(crate) fn in_pane(&self, pane_id: &str) -> Vec<Adoption> {
        let mut matches: Vec<_> = self
            .panes
            .values()
            .filter(|adoption| adoption.pane_id == pane_id)
            .cloned()
            .collect();
        matches.sort_by_key(|adoption| adoption.recipient);
        matches
    }

    /// Snapshot exact adoptions for roster rendering without nested locks.
    pub(crate) fn exact_adoptions(&self) -> Vec<Adoption> {
        self.panes.values().cloned().collect()
    }

    /// Exact adoption a label names. Legacy rows cannot resolve labels.
    pub(crate) fn for_label(&self, label: &str) -> Option<&Adoption> {
        self.panes.values().find(|adoption| adoption.label == label)
    }

    /// pane id -> label, for the surfaces that render the whole roster.
    pub(crate) fn labels(&self) -> HashMap<String, String> {
        let mut labels = HashMap::new();
        let mut ambiguous = std::collections::HashSet::new();
        for adoption in self.panes.values() {
            if ambiguous.contains(&adoption.pane_id) {
                continue;
            }
            if labels
                .insert(adoption.pane_id.clone(), adoption.label.clone())
                .is_some()
            {
                labels.remove(&adoption.pane_id);
                ambiguous.insert(adoption.pane_id.clone());
            }
        }
        labels
    }

    #[cfg(test)]
    pub(crate) fn get(&self, pane_id: &str) -> Option<&Adoption> {
        self.unique_for_pane(pane_id)
    }

    #[cfg(test)]
    fn unique_for_pane(&self, pane_id: &str) -> Option<&Adoption> {
        let mut matches = self
            .panes
            .values()
            .filter(|adoption| adoption.pane_id == pane_id);
        let found = matches.next()?;
        matches.next().is_none().then_some(found)
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

    /// Move every session-scoped registry fact to a session's new name.
    ///
    /// tmux pane and window ids do not change when their session is
    /// renamed, so the adoption itself and both chrome snapshots remain
    /// valid. Only their human-facing session name moves. Persist before
    /// committing, like [`Self::adopt`]: after an acknowledged rename the
    /// next daemon must restore the same roster under the name tmux now
    /// uses.
    pub(crate) fn rename_session(
        &mut self,
        old_name: &str,
        new_name: &str,
    ) -> Result<(), StateError> {
        if old_name == new_name {
            return Ok(());
        }
        let mut panes = self.panes.clone();
        let mut legacy_panes = self.legacy_panes.clone();
        let mut windows = self.windows.clone();
        let mut changed = false;
        for adoption in panes.values_mut().filter(|a| a.session == old_name) {
            adoption.session = new_name.to_string();
            changed = true;
        }
        for adoption in legacy_panes.iter_mut().filter(|a| a.session == old_name) {
            adoption.session = new_name.to_string();
            changed = true;
        }
        for window in windows.values_mut().filter(|w| w.session == old_name) {
            window.session = new_name.to_string();
            changed = true;
        }
        if !changed {
            return Ok(());
        }
        self.commit(panes, legacy_panes, windows)
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
    ) -> Result<(), StateError> {
        let mut panes = self.panes.clone();
        let mut legacy_panes = self.legacy_panes.clone();
        let mut windows = self.windows.clone();
        windows
            .entry(window.window_id.clone())
            .or_insert_with(|| window);
        let recipient = adoption
            .recipient
            .expect("new adoptions carry an exact recipient");
        assert!(
            adoption.pane_root.is_some(),
            "new adoptions carry an exact pane root"
        );
        legacy_panes.retain(|legacy| {
            legacy.session != adoption.session || legacy.pane_id != adoption.pane_id
        });
        panes.insert(recipient, adoption);
        self.commit(panes, legacy_panes, windows)
    }

    /// What [`clear`](Self::clear) would hand back, without handing it back.
    ///
    /// The chrome restore has to run BEFORE the entry is forgotten, because
    /// that entry is the only copy of the border settings tmux had before
    /// cyclops: commit the removal first and a failed restore destroys them.
    /// So the caller looks with this, restores, and only then clears.
    pub(crate) fn pending_clear(
        &self,
        recipient: RecipientKey,
        pane_root: ProcessInstanceId,
    ) -> Option<(Adoption, Option<WindowChrome>)> {
        let gone = self.for_route(recipient, pane_root)?.clone();
        // Same rule as the committed path, run on copies: `release_window`
        // counts an honest map, which means one with this pane already out.
        let mut panes = self.panes.clone();
        panes.remove(&recipient);
        let mut windows = self.windows.clone();
        let freed = release_window(&panes, &self.legacy_panes, &mut windows, &gone.window_id);
        Some((gone, freed))
    }

    /// Un-adopt a pane. Returns the entry that was there, plus the window
    /// snapshot when this was the last adopted pane in that window.
    pub(crate) fn clear(
        &mut self,
        recipient: RecipientKey,
        pane_root: ProcessInstanceId,
    ) -> Result<Option<(Adoption, Option<WindowChrome>)>, StateError> {
        let mut panes = self.panes.clone();
        let Some(gone) = panes
            .get(&recipient)
            .filter(|adoption| adoption.pane_root == Some(pane_root))
            .cloned()
        else {
            return Ok(None);
        };
        panes.remove(&recipient);
        let legacy_panes = self.legacy_panes.clone();
        let mut windows = self.windows.clone();
        let freed = release_window(&panes, &legacy_panes, &mut windows, &gone.window_id);
        self.commit(panes, legacy_panes, windows)?;
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
        recipient: RecipientKey,
        pane_root: ProcessInstanceId,
        destination: &str,
        destination_status: Option<String>,
    ) -> Result<Option<WindowChrome>, StateError> {
        let mut panes = self.panes.clone();
        let Some(moved) = panes
            .get_mut(&recipient)
            .filter(|adoption| adoption.pane_root == Some(pane_root))
        else {
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
        let legacy_panes = self.legacy_panes.clone();
        let freed = release_window(&panes, &legacy_panes, &mut windows, &source);
        self.commit(panes, legacy_panes, windows)?;
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
    /// Restore one session while retaining routes whose pane still exists
    /// elsewhere on the same tmux server.
    ///
    /// A session snapshot can disprove a route, not the physical pane. The
    /// caller proves the retained recipients server-wide before using this
    /// variant. They remain under their old durable key until the transfer is
    /// resolved, which prevents a local removal from destroying recovery
    /// identity or a pinned manifest.
    pub(crate) fn restore_session_preserving(
        &mut self,
        session: &str,
        session_instance_id: cyclops_proto::SessionInstanceId,
        live: &[(String, ProcessInstanceId)],
        retained: &HashSet<RecipientKey>,
    ) -> Result<Vec<Adoption>, StateError> {
        let mut panes = self.panes.clone();
        panes.retain(|recipient, a| {
            a.session != session
                || retained.contains(recipient)
                || (recipient.session_instance_id() == Some(session_instance_id)
                    && live
                        .iter()
                        .any(|(id, root)| *id == a.pane_id && a.pane_root == Some(*root)))
        });
        let legacy_panes = self.legacy_panes.clone();
        let mut windows = self.windows.clone();
        windows.retain(|id, w| {
            w.session != session
                || panes.values().any(|a| a.window_id == *id)
                || legacy_panes.iter().any(|a| a.window_id == *id)
        });
        let kept: Vec<Adoption> = {
            let mut v: Vec<Adoption> = panes
                .values()
                .filter(|a| {
                    a.session == session
                        && live
                            .iter()
                            .any(|(id, root)| *id == a.pane_id && a.pane_root == Some(*root))
                })
                .cloned()
                .collect();
            v.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
            v
        };
        self.commit(panes, legacy_panes, windows)?;
        Ok(kept)
    }

    /// A pane went away. Adoption ends with the pane (M1 rule); the entry
    /// and any window snapshot it was the last holder of go with it.
    pub(crate) fn forget(
        &mut self,
        recipient: RecipientKey,
        pane_root: ProcessInstanceId,
    ) -> Option<(Adoption, Option<WindowChrome>)> {
        match self.clear(recipient, pane_root) {
            Ok(gone) => gone,
            Err(e) => {
                // The pane is gone either way; refusing to forget it would
                // leave a dead label answering to messages. A failed commit
                // left the live maps untouched, so drop it from them here
                // and hand the window back by the same rule.
                tracing::error!(recipient = %recipient, error = %e, "registry write failed while forgetting a closed pane");
                let gone = self
                    .panes
                    .get(&recipient)
                    .filter(|adoption| adoption.pane_root == Some(pane_root))?
                    .clone();
                self.panes.remove(&recipient);
                let freed = release_window(
                    &self.panes,
                    &self.legacy_panes,
                    &mut self.windows,
                    &gone.window_id,
                );
                Some((gone, freed))
            }
        }
    }

    /// Write the candidate state, then take it. Order matters: a caller
    /// that saw Ok must be able to restart and find the same roster.
    fn commit(
        &mut self,
        panes: HashMap<RecipientKey, Adoption>,
        legacy_panes: Vec<Adoption>,
        windows: HashMap<String, WindowChrome>,
    ) -> Result<(), StateError> {
        let mut file = File {
            version: VERSION,
            panes: panes
                .values()
                .cloned()
                .chain(legacy_panes.iter().cloned())
                .collect(),
            windows: windows.values().cloned().collect(),
        };
        file.panes
            .sort_by(|a, b| (&a.session, &a.pane_id).cmp(&(&b.session, &b.pane_id)));
        file.windows.sort_by(|a, b| a.window_id.cmp(&b.window_id));
        let path = self.state_root.path().join(FILE);
        let mut text = serde_json::to_string_pretty(&file).map_err(|source| StateError::Io {
            path: path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
        })?;
        text.push('\n');
        self.state_root
            .replace_file(Path::new(FILE), text.as_bytes())?;
        self.panes = panes;
        self.legacy_panes = legacy_panes;
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
    panes: &HashMap<RecipientKey, Adoption>,
    legacy_panes: &[Adoption],
    windows: &mut HashMap<String, WindowChrome>,
    window_id: &str,
) -> Option<WindowChrome> {
    if panes.values().any(|a| a.window_id == window_id)
        || legacy_panes.iter().any(|a| a.window_id == window_id)
    {
        None
    } else {
        windows.remove(window_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{SessionInstanceId, TmuxPaneId, WorkspaceId};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn home(tag: &str) -> PathBuf {
        let dir = cyclops_proto::scratch::scratch_dir(&format!("cyc-reg-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch home");
        dir
    }

    fn load(home: &Path) -> (Registry, Vec<String>) {
        let state_root = Arc::new(StateRoot::open_or_create(home).unwrap());
        Registry::load(state_root)
    }

    fn adoption(pane: &str, label: &str, pid: i32, window: &str) -> Adoption {
        let pane_id: TmuxPaneId = pane.parse().unwrap();
        let pane_root = ProcessInstanceId::new(pid, pid as u64 + 10_000).unwrap();
        Adoption {
            session: "main".into(),
            pane_id: pane.into(),
            label: label.into(),
            recipient: Some(RecipientKey::agent(
                WorkspaceId::from_uuid(Uuid::from_u128(1)).unwrap(),
                test_session(),
                pane_id,
            )),
            pane_root: Some(pane_root),
            manifest: None,
            pane_pid: pid,
            window_id: window.into(),
            border_format: None,
        }
    }

    fn test_session() -> SessionInstanceId {
        SessionInstanceId::from_uuid(Uuid::from_u128(2)).unwrap()
    }

    fn route(adoption: &Adoption) -> (RecipientKey, ProcessInstanceId) {
        (adoption.recipient.unwrap(), adoption.pane_root.unwrap())
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
        let (mut reg, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut a = adoption("%1", "reviewer", 4242, "@0");
        a.manifest = Some("claude".into());
        a.border_format = Some("old-format".into());
        reg.adopt(a, window("@0", None)).expect("adopt writes");

        let (back, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back.label_of("%1").as_deref(), Some("reviewer"));
        assert_eq!(back.manifest_of("%1").as_deref(), Some("claude"));
        assert_eq!(
            back.for_label("reviewer").map(|a| a.pane_id.as_str()),
            Some("%1")
        );
        assert_eq!(
            back.get("%1").and_then(|a| a.border_format.clone()),
            Some("old-format".into())
        );
        assert!(back.window("@0").is_some());
        assert_eq!(
            std::fs::metadata(dir.join(FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!dir.join("registry.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_server_wide_session_transfer_survives_source_reconciliation() {
        let dir = home("cross-session-retain");
        let (mut reg, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        let adoption = adoption("%1", "reviewer", 4242, "@0");
        let (recipient, pane_root) = route(&adoption);
        reg.adopt(adoption, window("@0", None))
            .expect("adopt writes");

        let painted = reg
            .restore_session_preserving("main", test_session(), &[], &HashSet::from([recipient]))
            .expect("source reconciliation persists");

        assert!(
            painted.is_empty(),
            "a moved pane is not painted at its source"
        );
        assert!(reg.for_recipient(recipient).is_some());
        assert_eq!(
            reg.for_physical_pane("%1", pane_root)
                .map(|adoption| adoption.label.as_str()),
            Some("reviewer")
        );
        let (reopened, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(reopened.for_recipient(recipient).is_some());
    }

    /// A session rename changes no tmux identity: adopted panes and their
    /// window snapshots must move to the new session name together, in
    /// memory and on disk. Session-scoped repaint/restore paths otherwise
    /// stop finding them as soon as the watcher follows the rename.
    #[test]
    fn a_session_rename_moves_adoptions_and_window_snapshots_durably() {
        let dir = home("rename");
        let (mut reg, _) = load(&dir);
        reg.adopt(adoption("%1", "reviewer", 100, "@0"), window("@0", None))
            .unwrap();

        reg.rename_session("main", "renamed")
            .expect("rename writes");

        assert!(reg.in_session("main").is_empty());
        assert_eq!(
            reg.in_session("renamed")
                .iter()
                .map(|a| a.label.as_str())
                .collect::<Vec<_>>(),
            vec!["reviewer"]
        );
        assert_eq!(
            reg.window("@0").map(|window| window.session.as_str()),
            Some("renamed")
        );

        let (back, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(back.in_session("main").is_empty());
        assert_eq!(back.in_session("renamed").len(), 1);
        assert_eq!(
            back.window("@0").map(|window| window.session.as_str()),
            Some("renamed")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole reason the pid is stored. After a tmux server restart the
    /// ids start over, so an entry whose pane id is back with a different
    /// process is a different pane and must not inherit the name.
    #[test]
    fn restore_keeps_the_same_occupant_and_drops_everything_else() {
        let dir = home("restore");
        let (mut reg, _) = load(&dir);
        reg.adopt(adoption("%1", "reviewer", 100, "@0"), window("@0", None))
            .unwrap();
        reg.adopt(adoption("%2", "implementer", 200, "@0"), window("@0", None))
            .unwrap();
        reg.adopt(adoption("%3", "tests", 300, "@1"), window("@1", None))
            .unwrap();

        // %1 same pane, %2 same id with a new process, %3 gone entirely.
        let kept = reg
            .restore_session_preserving(
                "main",
                test_session(),
                &[
                    ("%1".into(), ProcessInstanceId::new(100, 10_100).unwrap()),
                    ("%2".into(), ProcessInstanceId::new(999, 10_999).unwrap()),
                ],
                &HashSet::new(),
            )
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

        let (back, _) = load(&dir);
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
        let (mut reg, _) = load(&dir);
        reg.adopt(adoption("%1", "a", 1, "@0"), window("@0", None))
            .unwrap();
        reg.adopt(adoption("%2", "b", 2, "@0"), window("@0", Some("top")))
            .unwrap();
        assert!(
            reg.window("@0").unwrap().border_status.is_none(),
            "the first snapshot is the one that was there before cyclops"
        );

        let first = reg.get("%1").cloned().unwrap();
        let (_, freed) = reg
            .clear(route(&first).0, route(&first).1)
            .unwrap()
            .expect("cleared");
        assert!(freed.is_none(), "%2 is still adopted in @0");
        assert!(reg.window("@0").is_some());

        let second = reg.get("%2").cloned().unwrap();
        let (_, freed) = reg
            .clear(route(&second).0, route(&second).1)
            .unwrap()
            .expect("cleared");
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
        let (mut reg, _) = load(&dir);
        reg.adopt(adoption("%1", "a", 1, "@0"), window("@0", None))
            .unwrap();
        reg.adopt(adoption("%2", "b", 2, "@0"), window("@0", None))
            .unwrap();

        // %1 leaves @0 for a window cyclops has never seen.
        let first = reg.get("%1").cloned().unwrap();
        let freed = reg
            .move_window(
                route(&first).0,
                route(&first).1,
                "@1",
                Some("bottom".into()),
            )
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
        let second = reg.get("%2").cloned().unwrap();
        let freed = reg
            .move_window(route(&second).0, route(&second).1, "@1", Some("top".into()))
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
        assert!(reg
            .move_window(route(&second).0, route(&second).1, "@1", None)
            .unwrap()
            .is_none());
        let absent = adoption("%9", "absent", 9, "@9");
        assert!(reg
            .move_window(route(&absent).0, route(&absent).1, "@1", None)
            .unwrap()
            .is_none());

        let (back, _) = load(&dir);
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
        let (mut reg, _) = load(&dir);
        reg.adopt(adoption("%1", "a", 1, "@0"), window("@0", Some("bottom")))
            .unwrap();
        reg.adopt(adoption("%2", "b", 2, "@0"), window("@0", None))
            .unwrap();

        // An unsafe target makes descriptor-bound replacement fail before
        // the registry maps commit.
        std::fs::remove_file(dir.join(FILE)).unwrap();
        std::fs::create_dir(dir.join(FILE)).unwrap();

        let first = reg.get("%1").cloned().unwrap();
        let (gone, freed) = reg
            .forget(route(&first).0, route(&first).1)
            .expect("the pane is forgotten either way");
        assert_eq!(gone.label, "a");
        assert!(freed.is_none(), "%2 is still adopted in @0");

        let second = reg.get("%2").cloned().unwrap();
        let (gone, freed) = reg
            .forget(route(&second).0, route(&second).1)
            .expect("forgotten");
        assert_eq!(gone.label, "b");
        assert_eq!(
            freed
                .expect("the last one out releases the window")
                .border_status
                .as_deref(),
            Some("bottom")
        );
        assert!(reg.window("@0").is_none());
        assert!(
            reg.forget(route(&first).0, route(&first).1).is_none(),
            "already gone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_legacy_pane_row_is_nonaddressable_until_exact_rebind() {
        let dir = home("legacy-nonaddressable");
        let mut legacy = adoption("%1", "legacy", 100, "@0");
        legacy.recipient = None;
        legacy.pane_root = None;
        let file = File {
            version: VERSION,
            panes: vec![legacy],
            windows: vec![window("@0", None)],
        };
        std::fs::write(
            dir.join(FILE),
            serde_json::to_vec_pretty(&file).expect("legacy registry serializes"),
        )
        .unwrap();

        let (mut reg, warnings) = load(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(reg.label_of("%1").is_none());
        assert!(reg.for_label("legacy").is_none());
        assert!(reg.labels().is_empty());

        let exact = adoption("%1", "worker", 100, "@0");
        let exact_route = route(&exact);
        reg.adopt(exact, window("@0", None)).unwrap();
        assert!(reg.for_route(exact_route.0, exact_route.1).is_some());
        assert_eq!(reg.label_of("%1").as_deref(), Some("worker"));

        let persisted: File =
            serde_json::from_slice(&std::fs::read(dir.join(FILE)).unwrap()).unwrap();
        assert_eq!(persisted.panes.len(), 1, "rebind replaces the legacy row");
        assert!(persisted.panes[0].recipient.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_warns_and_starts_empty() {
        let dir = home("broken");
        std::fs::write(dir.join(FILE), "{not json").unwrap();
        let (reg, warnings) = load(&dir);
        assert!(reg.labels().is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");

        std::fs::write(dir.join(FILE), r#"{"version":99,"panes":[]}"#).unwrap();
        let (reg, warnings) = load(&dir);
        assert!(reg.labels().is_empty());
        assert!(warnings[0].contains("version 99"), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn labels_are_unique_across_sessions() {
        let dir = home("unique");
        let (mut reg, _) = load(&dir);
        reg.adopt(adoption("%1", "reviewer", 1, "@0"), window("@0", None))
            .unwrap();
        let reviewer = reg.get("%1").unwrap().recipient.unwrap();
        assert_eq!(
            reg.for_label("reviewer").and_then(|a| a.recipient),
            Some(reviewer)
        );
        assert!(reg.for_label("implementer").is_none());
        assert_eq!(reg.sessions(), vec!["main"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
