//! Append-only NDJSON ledger (ADR-001, cmux events.jsonl pattern C6).
//!
//! One writer owns each record file and cyclopsd serializes writes through it.
//! Session delivery records tolerate malformed historical lines. The workspace
//! message journal uses strict replay. Both modes guarantee:
//!
//! - Every acknowledged append is fsynced before the call returns. A crash
//!   loses at most a line that was never acknowledged.
//! - `seq` is strictly monotonic per session file, across daemon restarts:
//!   recovery scans the tail and continues numbering. `boot_id` says which
//!   daemon run wrote a line.
//! - A torn final write is recovered on the next open. Lenient replay seals it
//!   and retains it when it validates, otherwise skips it. Strict replay
//!   removes it and rejects every complete error.
//! - Lines are never rewritten. Corrections are new lines.
//! - Every open is relative to a validated [`cyclops_state::StateRoot`].
//!
//! What it does not own: the shape of a line ([`cyclops_proto::LedgerLine`]),
//! which facts get written (cyclopsd), path security (`cyclops-state`), or any
//! query over the record. Reading is [`read_after`], a full scan from a cursor,
//! and that is on purpose: a 10k-line session ledger parses in single-digit
//! milliseconds on this machine, so filtering, folding, and paging live in
//! `cyclopsd/src/history.rs` where the read model does. An index here stays a
//! measured need rather than a speculative one.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cyclops_proto::LedgerLine;
use cyclops_state::{StateFile, StateRoot};

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger io at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    State(#[from] cyclops_state::StateError),
    #[error("ledger serialize: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("corrupt ledger line {line} at {path}: {reason}")]
    CorruptLine {
        path: PathBuf,
        line: usize,
        reason: String,
    },
    #[error("ledger sequence gap at line {line} in {path}: expected {expected}, found {found}")]
    SequenceGap {
        path: PathBuf,
        line: usize,
        expected: u64,
        found: u64,
    },
    #[error("ledger sequence is exhausted at {0}")]
    SequenceExhausted(PathBuf),
    #[error("ledger write state is unknown after an earlier io failure at {0}; reopen required")]
    WriteStateUnknown(PathBuf),
    #[error("ledger already has an active writer at {0}")]
    WriterInUse(PathBuf),
    #[error("ledger writer is sealed at {0}")]
    WriterSealed(PathBuf),
}

/// Crash-safe appender for one ledger file.
pub struct LedgerWriter {
    inner: Mutex<Inner>,
    path: PathBuf,
    boot_id: String,
}

struct Inner {
    file: StateFile,
    next_seq: u64,
    strict_replay: bool,
    write_state_unknown: bool,
    sealed: bool,
    locked: bool,
}

impl LedgerWriter {
    /// Open relative to `root`, seal a torn tail, and recover the next sequence.
    pub fn open(
        root: &StateRoot,
        descendant: &Path,
        boot_id: &str,
    ) -> Result<LedgerWriter, LedgerError> {
        Self::open_with_replay(root, descendant, boot_id, false).map(|(writer, _)| writer)
    }

    /// Open a ledger whose complete records must be contiguous and valid.
    /// An unterminated final write is removed before replay because append
    /// acknowledgment always follows the terminating newline and fsync.
    pub fn open_strict(
        root: &StateRoot,
        descendant: &Path,
        boot_id: &str,
    ) -> Result<LedgerWriter, LedgerError> {
        Self::open_with_replay(root, descendant, boot_id, true).map(|(writer, _)| writer)
    }

    /// Open a strict ledger and return the records validated during recovery.
    pub fn open_strict_with_replay(
        root: &StateRoot,
        descendant: &Path,
        boot_id: &str,
    ) -> Result<(LedgerWriter, Vec<LedgerLine>), LedgerError> {
        Self::open_with_replay(root, descendant, boot_id, true)
    }

    fn open_with_replay(
        root: &StateRoot,
        descendant: &Path,
        boot_id: &str,
        strict_replay: bool,
    ) -> Result<(LedgerWriter, Vec<LedgerLine>), LedgerError> {
        let path = root.path().join(descendant);
        let io = |source| LedgerError::Io {
            path: path.clone(),
            source,
        };
        let mut file = root.open_append(descendant)?;
        if !file.try_lock().map_err(io)? {
            return Err(LedgerError::WriterInUse(path));
        }

        // Recover only the final unterminated write. Complete records are
        // immutable, including complete records that fail strict replay.
        let len = file.seek(SeekFrom::End(0)).map_err(io)?;
        if len > 0 {
            file.seek(SeekFrom::End(-1)).map_err(io)?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last).map_err(io)?;
            if last[0] != b'\n' {
                if strict_replay {
                    file.seek(SeekFrom::Start(0)).map_err(io)?;
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes).map_err(io)?;
                    let retained = bytes
                        .iter()
                        .rposition(|byte| *byte == b'\n')
                        .map_or(0, |index| index + 1);
                    tracing::warn!(
                        path = %path.display(),
                        removed = bytes.len() - retained,
                        "removing unterminated ledger tail"
                    );
                    file.set_len(retained as u64).map_err(io)?;
                    file.sync_data().map_err(io)?;
                } else {
                    tracing::warn!(path = %path.display(), "sealing torn ledger tail");
                    file.write_all(b"\n").map_err(io)?;
                    file.sync_data().map_err(io)?;
                }
            }
        }

        // Recover sequence state from the same validated descriptor.
        file.seek(SeekFrom::Start(0)).map_err(io)?;
        let lines = if strict_replay {
            read_from_strict(BufReader::new(&mut file), &path, 0)?
        } else {
            read_from(BufReader::new(&mut file), &path, 0)?
        };
        let next_seq = match lines.last() {
            Some(line) => line
                .seq
                .checked_add(1)
                .ok_or_else(|| LedgerError::SequenceExhausted(path.clone()))?,
            None => 1,
        };

        // Hold that descriptor for every later append and replay.
        Ok((
            LedgerWriter {
                inner: Mutex::new(Inner {
                    file,
                    next_seq,
                    strict_replay,
                    write_state_unknown: false,
                    sealed: false,
                    locked: true,
                }),
                path,
                boot_id: boot_id.into(),
            },
            lines,
        ))
    }

    /// Display path only. Ledger I/O stays bound to the held descriptor.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// Assign sequence, boot id, and timestamp, then append and fsync. The
    /// returned line is what reached disk.
    pub fn append(&self, mut line: LedgerLine) -> Result<LedgerLine, LedgerError> {
        let io = |source| LedgerError::Io {
            path: self.path.clone(),
            source,
        };
        let mut inner = self.inner.lock().expect("ledger writer poisoned");
        if inner.sealed {
            return Err(LedgerError::WriterSealed(self.path.clone()));
        }
        if inner.write_state_unknown {
            return Err(LedgerError::WriteStateUnknown(self.path.clone()));
        }
        let following_seq = inner
            .next_seq
            .checked_add(1)
            .ok_or_else(|| LedgerError::SequenceExhausted(self.path.clone()))?;
        line.seq = inner.next_seq;
        line.boot_id = self.boot_id.clone();
        if line.ts == 0 {
            line.ts = now_ms();
        }
        let mut buffer = serde_json::to_vec(&line)?;
        buffer.push(b'\n');
        if let Err(source) = inner.file.write_all(&buffer) {
            inner.write_state_unknown = true;
            return Err(io(source));
        }
        if let Err(source) = inner.file.sync_data() {
            inner.write_state_unknown = true;
            return Err(io(source));
        }
        inner.next_seq = following_seq;
        Ok(line)
    }

    /// Replay from the same validated descriptor used for appends.
    pub fn read_after(&self, cursor: u64) -> Result<Vec<LedgerLine>, LedgerError> {
        let io = |source| LedgerError::Io {
            path: self.path.clone(),
            source,
        };
        let mut inner = self.inner.lock().expect("ledger writer poisoned");
        inner.file.seek(SeekFrom::Start(0)).map_err(io)?;
        if inner.strict_replay {
            read_from_strict(BufReader::new(&mut inner.file), &self.path, cursor)
        } else {
            read_from(BufReader::new(&mut inner.file), &self.path, cursor)
        }
    }

    /// The sequence assigned to the next append.
    pub fn next_seq(&self) -> u64 {
        self.inner.lock().expect("ledger writer poisoned").next_seq
    }

    /// Stop future appends and release this writer's lifetime file lease.
    ///
    /// The writer mutex makes sealing an exact boundary. An append either
    /// finishes before the lease is released or observes the sealed state.
    pub fn seal(&self) -> Result<(), LedgerError> {
        let io = |source| LedgerError::Io {
            path: self.path.clone(),
            source,
        };
        let mut inner = self.inner.lock().expect("ledger writer poisoned");
        inner.sealed = true;
        if inner.locked {
            inner.file.unlock().map_err(io)?;
            inner.locked = false;
        }
        Ok(())
    }
}

/// Replay valid lines with `seq > cursor` from a validated state descendant.
pub fn read_after(
    root: &StateRoot,
    descendant: &Path,
    cursor: u64,
) -> Result<Vec<LedgerLine>, LedgerError> {
    let path = root.path().join(descendant);
    let Some(file) = root.open_read(descendant)? else {
        return Ok(Vec::new());
    };
    read_from(BufReader::new(file), &path, cursor)
}

fn read_from(
    reader: impl BufRead,
    path: &Path,
    cursor: u64,
) -> Result<Vec<LedgerLine>, LedgerError> {
    let io = |source| LedgerError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut lines = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(io)?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LedgerLine>(&line) {
            Ok(line) if line.seq > cursor => lines.push(line),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    line = index + 1,
                    %error,
                    "skipping invalid ledger line"
                );
            }
        }
    }
    Ok(lines)
}

fn read_from_strict(
    reader: impl BufRead,
    path: &Path,
    cursor: u64,
) -> Result<Vec<LedgerLine>, LedgerError> {
    let io = |source| LedgerError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut lines = Vec::new();
    let mut expected = 1u64;
    for (index, raw) in reader.lines().enumerate() {
        let raw = raw.map_err(io)?;
        let physical_line = index + 1;
        if raw.trim().is_empty() {
            return Err(LedgerError::CorruptLine {
                path: path.to_path_buf(),
                line: physical_line,
                reason: "empty record".into(),
            });
        }
        let line =
            serde_json::from_str::<LedgerLine>(&raw).map_err(|error| LedgerError::CorruptLine {
                path: path.to_path_buf(),
                line: physical_line,
                reason: error.to_string(),
            })?;
        if line.seq != expected {
            return Err(LedgerError::SequenceGap {
                path: path.to_path_buf(),
                line: physical_line,
                expected,
                found: line.seq,
            });
        }
        expected = expected
            .checked_add(1)
            .ok_or_else(|| LedgerError::SequenceExhausted(path.to_path_buf()))?;
        if line.seq > cursor {
            lines.push(line);
        }
    }
    Ok(lines)
}

/// Highest valid sequence in a ledger, if any.
pub fn last_valid_seq(root: &StateRoot, descendant: &Path) -> Result<Option<u64>, LedgerError> {
    Ok(read_after(root, descendant, 0)?.last().map(|line| line.seq))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{
        scratch::{scratch_dir, scratch_root},
        Kind, LedgerLine,
    };
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

    struct Scratch {
        path: PathBuf,
        root: StateRoot,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
            let path = scratch_dir(&format!("cyclops-ledger-{tag}-{}-{sequence}", now_ms()));
            assert!(path.starts_with(scratch_root()));
            let root = StateRoot::open_or_create(&path).unwrap();
            Self { path, root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    panic!("remove scratch {}: {error}", self.path.display());
                }
            }
        }
    }

    fn message(subject: &str) -> LedgerLine {
        LedgerLine {
            seq: 0,
            boot_id: String::new(),
            id: format!("m-{subject}"),
            ts: 0,
            kind: Kind::Msg,
            from: "codex".into(),
            to: vec!["reviewer".into()],
            subject: Some(subject.into()),
            body: None,
            reply_to: None,
            deliveries: vec![],
            data: None,
        }
    }

    #[test]
    fn append_assigns_and_replays() {
        let scratch = Scratch::new("append");
        let descendant = Path::new("ledger/main.ndjson");
        let writer = LedgerWriter::open(&scratch.root, descendant, "boot-1").unwrap();
        let first = writer.append(message("one")).unwrap();
        let second = writer.append(message("two")).unwrap();
        assert_eq!((first.seq, second.seq), (1, 2));
        assert!(first.ts > 0);
        assert_eq!(writer.read_after(0).unwrap().len(), 2);
        assert_eq!(
            writer.read_after(1).unwrap()[0].subject.as_deref(),
            Some("two")
        );
    }

    #[test]
    fn one_writer_owns_the_ledger_until_an_exact_seal() {
        let scratch = Scratch::new("writer-lease");
        let descendant = Path::new("ledger/main.ndjson");
        let first = LedgerWriter::open(&scratch.root, descendant, "boot-1").unwrap();
        first.append(message("one")).unwrap();
        let before = fs::read(scratch.root.path().join(descendant)).unwrap();

        assert!(matches!(
            LedgerWriter::open(&scratch.root, descendant, "boot-2"),
            Err(LedgerError::WriterInUse(_))
        ));
        assert_eq!(
            fs::read(scratch.root.path().join(descendant)).unwrap(),
            before
        );

        first.seal().unwrap();
        assert!(matches!(
            first.append(message("late")),
            Err(LedgerError::WriterSealed(_))
        ));
        let second = LedgerWriter::open(&scratch.root, descendant, "boot-2").unwrap();
        assert_eq!(second.append(message("two")).unwrap().seq, 2);
    }

    #[test]
    fn sequence_continues_across_reopen() {
        let scratch = Scratch::new("reopen");
        let descendant = Path::new("ledger/main.ndjson");
        {
            let writer = LedgerWriter::open(&scratch.root, descendant, "boot-1").unwrap();
            writer.append(message("one")).unwrap();
        }
        let writer = LedgerWriter::open(&scratch.root, descendant, "boot-2").unwrap();
        let line = writer.append(message("two")).unwrap();
        assert_eq!(line.seq, 2);
        assert_eq!(line.boot_id, "boot-2");
        let all = writer.read_after(0).unwrap();
        assert_eq!(all[0].boot_id, "boot-1");
        assert_eq!(all[1].boot_id, "boot-2");
    }

    #[test]
    fn torn_tail_is_sealed_and_skipped() {
        let scratch = Scratch::new("torn-tail");
        let descendant = Path::new("ledger/main.ndjson");
        {
            let writer = LedgerWriter::open(&scratch.root, descendant, "boot-1").unwrap();
            writer.append(message("one")).unwrap();
        }
        {
            let mut file = scratch.root.open_append(descendant).unwrap();
            file.write_all(br#"{"seq":2,"boot_id":"boot-1","id":"m-torn","ts":9,"ki"#)
                .unwrap();
        }
        let writer = LedgerWriter::open(&scratch.root, descendant, "boot-2").unwrap();
        assert_eq!(writer.append(message("three")).unwrap().seq, 2);
        let lines = writer.read_after(0).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].subject.as_deref(), Some("three"));
    }

    #[test]
    fn lenient_replay_seals_and_retains_a_valid_unterminated_tail() {
        let scratch = Scratch::new("valid-torn-tail");
        let descendant = Path::new("ledger/main.ndjson");
        {
            let writer = LedgerWriter::open(&scratch.root, descendant, "boot-1").unwrap();
            writer.append(message("one")).unwrap();
        }
        {
            let mut second = message("two");
            second.seq = 2;
            second.boot_id = "boot-1".into();
            second.ts = 9;
            let mut file = scratch.root.open_append(descendant).unwrap();
            serde_json::to_writer(&mut file, &second).unwrap();
            file.sync_data().unwrap();
        }

        let writer = LedgerWriter::open(&scratch.root, descendant, "boot-2").unwrap();
        assert_eq!(writer.next_seq(), 3);
        let lines = writer.read_after(0).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].subject.as_deref(), Some("two"));
        assert_eq!(
            fs::read(scratch.root.path().join(descendant))
                .unwrap()
                .last(),
            Some(&b'\n')
        );
    }

    #[test]
    fn missing_file_reads_empty() {
        let scratch = Scratch::new("missing");
        let descendant = Path::new("ledger/missing.ndjson");
        assert!(read_after(&scratch.root, descendant, 0).unwrap().is_empty());
        assert!(!scratch.root.path().join(descendant).exists());
    }

    #[test]
    fn invalid_middle_line_does_not_abort_replay() {
        let scratch = Scratch::new("invalid-middle");
        let descendant = Path::new("ledger/main.ndjson");
        {
            let writer = LedgerWriter::open(&scratch.root, descendant, "boot").unwrap();
            writer.append(message("one")).unwrap();
        }
        {
            let mut file = scratch.root.open_append(descendant).unwrap();
            file.write_all(b"not json at all\n").unwrap();
        }
        let writer = LedgerWriter::open(&scratch.root, descendant, "boot").unwrap();
        writer.append(message("two")).unwrap();
        assert_eq!(writer.read_after(0).unwrap().len(), 2);
    }

    #[test]
    fn strict_replay_refuses_complete_corruption() {
        let scratch = Scratch::new("strict-corruption");
        let descendant = Path::new("messages/workspace.ndjson");
        {
            let writer = LedgerWriter::open_strict(&scratch.root, descendant, "boot").unwrap();
            writer.append(message("one")).unwrap();
        }
        {
            let mut file = scratch.root.open_append(descendant).unwrap();
            file.write_all(b"not json\n").unwrap();
            file.sync_data().unwrap();
        }

        assert!(matches!(
            LedgerWriter::open_strict(&scratch.root, descendant, "boot"),
            Err(LedgerError::CorruptLine { line: 2, .. })
        ));
    }

    #[test]
    fn strict_replay_refuses_sequence_gaps() {
        let scratch = Scratch::new("strict-gap");
        let descendant = Path::new("messages/workspace.ndjson");
        let mut second = message("two");
        second.seq = 3;
        second.boot_id = "boot".into();
        second.ts = 1;
        {
            let writer = LedgerWriter::open_strict(&scratch.root, descendant, "boot").unwrap();
            writer.append(message("one")).unwrap();
        }
        {
            let mut file = scratch.root.open_append(descendant).unwrap();
            serde_json::to_writer(&mut file, &second).unwrap();
            file.write_all(b"\n").unwrap();
            file.sync_data().unwrap();
        }

        assert!(matches!(
            LedgerWriter::open_strict(&scratch.root, descendant, "boot"),
            Err(LedgerError::SequenceGap {
                line: 2,
                expected: 2,
                found: 3,
                ..
            })
        ));
    }

    #[test]
    fn strict_replay_removes_only_an_unterminated_tail() {
        let scratch = Scratch::new("strict-torn-tail");
        let descendant = Path::new("messages/workspace.ndjson");
        {
            let writer = LedgerWriter::open_strict(&scratch.root, descendant, "boot-1").unwrap();
            writer.append(message("one")).unwrap();
        }
        {
            let mut file = scratch.root.open_append(descendant).unwrap();
            file.write_all(br#"{"seq":2,"boot_id":"boot-1","id":"m-torn""#)
                .unwrap();
            file.sync_data().unwrap();
        }

        let writer = LedgerWriter::open_strict(&scratch.root, descendant, "boot-2").unwrap();
        assert_eq!(writer.append(message("two")).unwrap().seq, 2);
        let lines = writer.read_after(0).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].subject.as_deref(), Some("one"));
        assert_eq!(lines[1].subject.as_deref(), Some("two"));
    }

    #[test]
    fn strict_open_returns_the_records_used_for_sequence_recovery() {
        let scratch = Scratch::new("strict-returned-replay");
        let descendant = Path::new("messages/workspace.ndjson");
        {
            let writer = LedgerWriter::open_strict(&scratch.root, descendant, "boot-1").unwrap();
            writer.append(message("one")).unwrap();
            writer.append(message("two")).unwrap();
        }

        let (writer, lines) =
            LedgerWriter::open_strict_with_replay(&scratch.root, descendant, "boot-2").unwrap();
        assert_eq!(writer.next_seq(), 3);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].subject.as_deref(), Some("one"));
        assert_eq!(lines[1].subject.as_deref(), Some("two"));
    }

    #[test]
    fn ledger_paths_are_relative_to_the_state_root() {
        let scratch = Scratch::new("containment");
        let outside = scratch.path.parent().unwrap().join("outside-ledger");
        fs::write(&outside, b"keep").unwrap();
        let before = fs::read(&outside).unwrap();

        assert!(LedgerWriter::open(&scratch.root, Path::new("../outside-ledger"), "boot").is_err());
        assert_eq!(fs::read(&outside).unwrap(), before);
        let _ = fs::remove_file(outside);
    }

    #[test]
    fn recipient_path_traversal_cannot_reach_another_ledger() {
        let scratch = Scratch::new("recipient-containment");
        let reviewer = Path::new("ledger/reviewer.ndjson");
        {
            let writer = LedgerWriter::open(&scratch.root, reviewer, "boot").unwrap();
            writer.append(message("reviewer-only")).unwrap();
        }
        let path = scratch.root.path().join(reviewer);
        let before = fs::read(&path).unwrap();
        let before_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        let crossing = Path::new("ledger/implementer/../../ledger/reviewer.ndjson");
        assert!(LedgerWriter::open(&scratch.root, crossing, "boot").is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            before_mode
        );
    }

    #[test]
    fn ledger_integration_keeps_owner_only_modes() {
        let scratch = Scratch::new("modes");
        let descendant = Path::new("ledger/main.ndjson");
        let _ = LedgerWriter::open(&scratch.root, descendant, "boot").unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(scratch.root.path()), 0o700);
        assert_eq!(mode(&scratch.root.path().join("ledger")), 0o700);
        assert_eq!(mode(&scratch.root.path().join(descendant)), 0o600);
    }
}
