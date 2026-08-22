//! The durable record of which live session got which durable name.
//!
//! [`crate::sessionid`] enforces the bijection in memory. Nothing in
//! memory survives a restart, and a name minted twice for one session is
//! the failure the registry exists to prevent, so the assignments are
//! written down and read back.

use cyclops_proto::{SessionIdentityBinding, SessionInstanceId};
use cyclops_state::{StateError, StateRoot};

use crate::sessionid::{BindError, Resolved, SessionIdentityRegistry};

/// Where the assignments live under the state root.
const RECORD: &str = "identity/sessions.ndjson";

#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error("session identity record: {0}")]
    State(#[from] StateError),
    #[error("session identity record line {line}: {why}")]
    Unreadable { line: usize, why: String },
    #[error("session identity record line {line}: {source}")]
    Conflict {
        line: usize,
        #[source]
        source: BindError,
    },
    #[error("session identity record: {0}")]
    Rejected(#[from] BindError),
}

/// The registry and its durable record, kept in step.
///
/// The two move together or not at all. A name handed out and then lost
/// to a failed write is worse than no name: the daemon routes under an
/// identity that vanishes at the next restart.
pub(crate) struct SessionIdentities {
    live: SessionIdentityRegistry,
}

impl SessionIdentities {
    /// Read the assignments back, or refuse.
    ///
    /// A record that cannot be read whole is a refusal rather than a
    /// partial load. Skipping a line silently drops an assignment, and
    /// the next resolve for that session mints a second name for it,
    /// which is what writing the record down was meant to prevent.
    pub(crate) fn open(root: &StateRoot) -> Result<SessionIdentities, StoreError> {
        let mut live = SessionIdentityRegistry::new();
        let Some(mut file) = root.open_read(std::path::Path::new(RECORD))? else {
            return Ok(SessionIdentities { live });
        };
        let mut text = String::new();
        {
            use std::io::Read;
            file.read_to_string(&mut text)
                .map_err(|e| StoreError::Unreadable {
                    line: 0,
                    why: e.to_string(),
                })?;
        }
        for (index, line) in text.lines().enumerate() {
            let line_no = index + 1;
            if line.trim().is_empty() {
                continue;
            }
            let binding: SessionIdentityBinding =
                serde_json::from_str(line).map_err(|e| StoreError::Unreadable {
                    line: line_no,
                    why: e.to_string(),
                })?;
            live.bind(&binding).map_err(|source| StoreError::Conflict {
                line: line_no,
                source,
            })?;
        }
        Ok(SessionIdentities { live })
    }

    /// The durable name for a live session, minting and recording one
    /// only if this session has never been seen.
    ///
    /// Prepared on a copy, written, and only then adopted. Mutating the
    /// live registry first would hand out a name that the record does not
    /// have if the write fails, and the daemon would route under an
    /// identity that disappears at the next restart.
    pub(crate) fn resolve(
        &mut self,
        root: &StateRoot,
        key: &cyclops_proto::LiveSessionKey,
        mint: impl FnOnce() -> SessionInstanceId,
    ) -> Result<SessionInstanceId, StoreError> {
        let mut candidate = self.live.clone();
        match candidate.resolve(key, mint)? {
            // Already recorded; nothing to write.
            Resolved::Existing(id) => Ok(id),
            Resolved::Minted(id) => {
                save(root, &candidate)?;
                self.live = candidate;
                Ok(id)
            }
        }
    }

    /// Whether two durable sessions belong to one live tmux server generation.
    ///
    /// Session IDs differ when a pane moves between sessions. The OS boot and
    /// tmux server process identify the server across that transfer.
    pub(crate) fn same_tmux_server_generation(
        &self,
        left: SessionInstanceId,
        right: SessionInstanceId,
    ) -> bool {
        let Some(left) = self.live.live_session_of(left) else {
            return false;
        };
        let Some(right) = self.live.live_session_of(right) else {
            return false;
        };
        left.workspace_id() == right.workspace_id()
            && left.os_boot_id() == right.os_boot_id()
            && left.tmux_server() == right.tmux_server()
    }

    #[cfg(test)]
    pub(crate) fn instance_of(
        &self,
        key: &cyclops_proto::LiveSessionKey,
    ) -> Option<SessionInstanceId> {
        self.live.instance_of(key)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.live.len()
    }
}

/// Write every assignment, replacing the record atomically.
///
/// Whole-file replacement rather than an append: the registry is the one
/// authority for what the set contains, and a record built by appending
/// can disagree with it after any partial write.
fn save(root: &StateRoot, registry: &SessionIdentityRegistry) -> Result<(), StoreError> {
    let mut out = String::new();
    for binding in registry.bindings() {
        out.push_str(&serde_json::to_string(&binding).expect("binding serializes"));
        out.push('\n');
    }
    root.replace_file(std::path::Path::new(RECORD), out.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{LiveSessionKey, OsBootId, ProcessInstanceId, WorkspaceId};

    /// Any workspace: these tests are about the record under one, not
    /// about how a workspace gets its name.
    fn workspace() -> WorkspaceId {
        "11111111-1111-4111-8111-111111111111"
            .parse()
            .expect("workspace id")
    }

    fn state_root() -> (StateRoot, tempdir::Scratch) {
        let dir = tempdir::Scratch::new();
        let root = StateRoot::open_or_create(dir.path()).expect("state root");
        (root, dir)
    }

    /// A scratch directory of its own, removed on drop.
    ///
    /// Its own, because the tests run in parallel in one process and the
    /// state root refuses a target that changed under it: sharing one
    /// makes them fail on each other rather than on the behaviour under
    /// test.
    mod tempdir {
        use std::sync::atomic::{AtomicU32, Ordering};

        static NEXT: AtomicU32 = AtomicU32::new(0);

        pub struct Scratch(std::path::PathBuf);

        impl Scratch {
            pub fn new() -> Scratch {
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let path = cyclops_proto::scratch::scratch_dir(&format!("cyc-sessionstore-{n}"));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).expect("scratch dir");
                Scratch(path)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn instance(n: u8) -> SessionInstanceId {
        format!("22222222-2222-4222-8222-2222222222{n:02}")
            .parse()
            .expect("session instance id")
    }

    fn key(workspace: WorkspaceId, session: &str) -> LiveSessionKey {
        LiveSessionKey::new(
            workspace,
            OsBootId::new("boot-a").expect("os boot id"),
            ProcessInstanceId::new(900, 1000).expect("tmux server id"),
            session.parse().expect("tmux session id"),
        )
    }

    fn record(root: &StateRoot) -> String {
        std::fs::read_to_string(root.path().join(RECORD)).expect("read record")
    }

    /// The point of writing it down: a session keeps its name across a
    /// restart, so nothing filed under the old name is stranded.
    #[test]
    fn a_name_survives_a_restart() {
        let (root, _dir) = state_root();
        let ws = workspace();
        let mut before = SessionIdentities::open(&root).expect("opened");
        before
            .resolve(&root, &key(ws, "$1"), || instance(1))
            .expect("minted");
        before
            .resolve(&root, &key(ws, "$2"), || instance(2))
            .expect("minted");

        let mut after = SessionIdentities::open(&root).expect("reopened");
        assert_eq!(after.len(), 2);
        assert_eq!(after.instance_of(&key(ws, "$1")), Some(instance(1)));
        assert_eq!(
            after
                .resolve(&root, &key(ws, "$1"), || panic!("minted over a saved name"))
                .expect("resolved"),
            instance(1)
        );
    }

    #[test]
    fn server_generation_ignores_session_moves_but_not_server_replacement() {
        let (root, _dir) = state_root();
        let ws = workspace();
        let first = key(ws, "$1");
        let moved = key(ws, "$2");
        let replacement = LiveSessionKey::new(
            ws,
            OsBootId::new("boot-a").unwrap(),
            ProcessInstanceId::new(901, 2000).unwrap(),
            "$3".parse().unwrap(),
        );
        let rebooted = LiveSessionKey::new(
            ws,
            OsBootId::new("boot-b").unwrap(),
            ProcessInstanceId::new(900, 1000).unwrap(),
            "$4".parse().unwrap(),
        );
        let mut ids = SessionIdentities::open(&root).unwrap();
        let first_id = ids.resolve(&root, &first, || instance(1)).unwrap();
        let moved_id = ids.resolve(&root, &moved, || instance(2)).unwrap();
        let replacement_id = ids.resolve(&root, &replacement, || instance(3)).unwrap();
        let rebooted_id = ids.resolve(&root, &rebooted, || instance(4)).unwrap();

        assert!(ids.same_tmux_server_generation(first_id, moved_id));
        assert!(!ids.same_tmux_server_generation(first_id, replacement_id));
        assert!(!ids.same_tmux_server_generation(first_id, rebooted_id));
    }

    /// A name is never handed out before the record has it.
    ///
    /// Mutating memory first and writing second leaves the daemon routing
    /// under an identity the next restart does not have. The write fails
    /// here because the record path is a directory, which is a real
    /// refusal from the state root rather than a simulated one.
    #[test]
    fn a_name_the_record_could_not_take_is_not_handed_out() {
        let (root, dir) = state_root();
        let ws = workspace();
        let mut ids = SessionIdentities::open(&root).expect("opened");
        std::fs::create_dir_all(dir.path().join(RECORD)).expect("block the record path");

        assert!(ids.resolve(&root, &key(ws, "$1"), || instance(1)).is_err());
        // Nothing was adopted, so the next attempt is still a first one.
        assert_eq!(ids.len(), 0);
        assert_eq!(ids.instance_of(&key(ws, "$1")), None);

        // And once the record can be written, the same session mints.
        std::fs::remove_dir(dir.path().join(RECORD)).expect("unblock");
        assert_eq!(
            ids.resolve(&root, &key(ws, "$1"), || instance(1))
                .expect("minted"),
            instance(1)
        );
        assert_eq!(SessionIdentities::open(&root).expect("reopened").len(), 1);
    }

    /// The same set is the same bytes, whatever order it was built in.
    /// An order that follows hash iteration makes a real change
    /// indistinguishable from noise.
    #[test]
    fn the_record_does_not_churn() {
        let (a, _a_dir) = state_root();
        let (b, _b_dir) = state_root();
        let ws = workspace();

        let mut forward = SessionIdentities::open(&a).expect("opened");
        let mut backward = SessionIdentities::open(&b).expect("opened");
        for (i, session) in ["$1", "$2", "$3"].iter().enumerate() {
            forward
                .resolve(&a, &key(ws, session), || instance(i as u8 + 1))
                .expect("minted");
        }
        for (i, session) in ["$3", "$2", "$1"].iter().enumerate() {
            backward
                .resolve(&b, &key(ws, session), || instance(3 - i as u8))
                .expect("minted");
        }
        assert_eq!(record(&a), record(&b));

        // And rewriting an unchanged set rewrites the same bytes.
        let before = record(&a);
        forward
            .resolve(&a, &key(ws, "$1"), || panic!("already named"))
            .expect("resolved");
        assert_eq!(record(&a), before);
    }

    /// No record yet is a workspace that has never minted, not an error.
    #[test]
    fn an_absent_record_loads_empty() {
        let (root, _dir) = state_root();
        assert_eq!(SessionIdentities::open(&root).expect("opened").len(), 0);
    }

    /// A record that cannot be read whole is a refusal.
    ///
    /// Skipping a bad line drops an assignment, and the next resolve for
    /// that session mints a second name for it: the exact failure writing
    /// the record down was meant to prevent.
    #[test]
    fn a_damaged_record_refuses_rather_than_dropping_a_name() {
        let (root, _dir) = state_root();
        let ws = workspace();
        let mut ids = SessionIdentities::open(&root).expect("opened");
        ids.resolve(&root, &key(ws, "$1"), || instance(1))
            .expect("minted");
        let good = record(&root);

        for (case, contents) in [
            ("truncated line", format!("{}{}", good, r#"{"live_sess"#)),
            ("not json", format!("{good}nonsense\n")),
            ("empty object", format!("{good}{{}}\n")),
        ] {
            root.replace_file(std::path::Path::new(RECORD), contents.as_bytes())
                .expect("wrote damaged record");
            assert!(
                matches!(
                    SessionIdentities::open(&root),
                    Err(StoreError::Unreadable { .. })
                ),
                "{case} was accepted"
            );
        }
    }

    /// A record holding two names for one session, or one name for two
    /// sessions, is refused with the line that broke it.
    #[test]
    fn a_record_that_breaks_the_bijection_refuses() {
        let (root, _dir) = state_root();
        let ws = workspace();
        let forward = [
            SessionIdentityBinding::new(key(ws, "$1"), instance(1)),
            SessionIdentityBinding::new(key(ws, "$1"), instance(2)),
        ];
        let reverse = [
            SessionIdentityBinding::new(key(ws, "$1"), instance(1)),
            SessionIdentityBinding::new(key(ws, "$2"), instance(1)),
        ];
        for (case, bindings) in [("two names", forward), ("two sessions", reverse)] {
            let contents: String = bindings
                .iter()
                .map(|b| format!("{}\n", serde_json::to_string(b).expect("json")))
                .collect();
            root.replace_file(std::path::Path::new(RECORD), contents.as_bytes())
                .expect("wrote record");
            match SessionIdentities::open(&root) {
                Err(StoreError::Conflict { line, .. }) => {
                    assert_eq!(line, 2, "{case}: wrong line blamed")
                }
                other => panic!("{case}: {} was accepted", other.is_ok()),
            }
        }
    }

    /// Blank lines are whitespace, not assignments.
    #[test]
    fn blank_lines_are_not_assignments() {
        let (root, _dir) = state_root();
        let ws = workspace();
        let binding = SessionIdentityBinding::new(key(ws, "$1"), instance(1));
        let contents = format!("\n{}\n\n", serde_json::to_string(&binding).expect("json"));
        root.replace_file(std::path::Path::new(RECORD), contents.as_bytes())
            .expect("wrote record");
        assert_eq!(SessionIdentities::open(&root).expect("opened").len(), 1);
    }
}
