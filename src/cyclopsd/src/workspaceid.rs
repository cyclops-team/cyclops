//! Durable identity for the state domain rooted at one Cyclops home.

use std::io::Read as _;
use std::path::Path;

use cyclops_proto::WorkspaceId;
use cyclops_state::{StateError, StateRoot};

const RECORD: &str = "identity/workspace-id";
const MAX_RECORD_BYTES: u64 = 64;

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkspaceIdentityError {
    #[error("workspace identity: {0}")]
    State(#[from] StateError),
    #[error("workspace identity record is missing after creation")]
    Missing,
    #[error("workspace identity record is too large")]
    TooLarge,
    #[error("workspace identity record: {0}")]
    Invalid(#[from] cyclops_proto::IdentityError),
    #[error("workspace identity record: {0}")]
    Io(#[from] std::io::Error),
}

/// Return the one workspace identity stored under this state root.
///
/// Concurrent first boots may propose different identifiers. The secure
/// create-once primitive publishes one, and every caller reads that winner.
pub(crate) fn load_or_create(root: &StateRoot) -> Result<WorkspaceId, WorkspaceIdentityError> {
    let candidate = WorkspaceId::from_uuid(uuid::Uuid::new_v4())?;
    let contents = format!("{candidate}\n");
    let _ = root.create_file_once(Path::new(RECORD), contents.as_bytes())?;

    let mut file = root
        .open_read(Path::new(RECORD))?
        .ok_or(WorkspaceIdentityError::Missing)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(WorkspaceIdentityError::TooLarge);
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let canonical = text.strip_suffix('\n').unwrap_or(text);
    Ok(canonical.parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn root(tag: &str) -> (Arc<StateRoot>, std::path::PathBuf) {
        let path = cyclops_proto::scratch::scratch_dir(&format!(
            "workspace-identity-{tag}-{}",
            uuid::Uuid::new_v4()
        ));
        let root = Arc::new(StateRoot::open_or_create(&path).unwrap());
        (root, path)
    }

    #[test]
    fn concurrent_first_boots_read_one_identity() {
        let (root, path) = root("race");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let root = Arc::clone(&root);
                std::thread::spawn(move || load_or_create(&root).unwrap())
            })
            .collect();
        let ids: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(ids.iter().all(|id| *id == ids[0]));
        assert_eq!(load_or_create(&root).unwrap(), ids[0]);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn independent_first_boots_read_the_same_identity() {
        let path = cyclops_proto::scratch::scratch_dir(&format!(
            "workspace-identity-independent-{}",
            uuid::Uuid::new_v4()
        ));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let root = StateRoot::open_or_create(&path).unwrap();
                    load_or_create(&root).unwrap()
                })
            })
            .collect();
        let ids: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(ids.iter().all(|id| *id == ids[0]));
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn corrupt_identity_refuses_without_replacing_it() {
        let (root, path) = root("corrupt");
        root.create_file_once(Path::new(RECORD), b"not-an-id\n")
            .unwrap();
        assert!(matches!(
            load_or_create(&root),
            Err(WorkspaceIdentityError::Invalid(_))
        ));
        assert_eq!(std::fs::read(path.join(RECORD)).unwrap(), b"not-an-id\n");
        std::fs::remove_dir_all(path).unwrap();
    }
}
