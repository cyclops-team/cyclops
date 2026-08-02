//! Append-only NDJSON ledger (ADR-001, cmux events.jsonl pattern C6).
//!
//! One file per watched session. The writer is the only appender; cyclopsd
//! serializes all writes through it. Guarantees:
//!
//! - Every acknowledged append is fsynced before the call returns. A crash
//!   loses at most a line that was never acknowledged (GOALS: ledger zero
//!   loss across crashes).
//! - `seq` is strictly monotonic per session file, across daemon restarts:
//!   recovery scans the tail and continues numbering. `boot_id` says which
//!   daemon run wrote a line.
//! - A torn final line (crash mid-write) is sealed on the next open by
//!   appending a newline; readers skip unparseable lines with a warning and
//!   never abort a replay over one.
//! - Lines are never rewritten. Corrections are new lines.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cyclops_proto::LedgerLine;

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger io at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("ledger serialize: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Crash-safe appender for one session's ledger file.
pub struct LedgerWriter {
    inner: Mutex<Inner>,
    path: PathBuf,
    boot_id: String,
}

struct Inner {
    file: File,
    next_seq: u64,
}

impl LedgerWriter {
    /// Open (creating parents) and recover: seal a torn tail, find the last
    /// valid seq, continue numbering after it.
    pub fn open(path: &Path, boot_id: &str) -> Result<LedgerWriter, LedgerError> {
        let io = |source| LedgerError::Io { path: path.into(), source };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .map_err(io)?;

        // Seal a torn final line so the next append starts on its own line.
        // The torn line becomes a complete-but-invalid line readers skip.
        let len = file.seek(SeekFrom::End(0)).map_err(io)?;
        if len > 0 {
            file.seek(SeekFrom::End(-1)).map_err(io)?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last).map_err(io)?;
            if last[0] != b'\n' {
                tracing::warn!(path = %path.display(), "sealing torn ledger tail");
                file.write_all(b"\n").map_err(io)?;
                file.sync_data().map_err(io)?;
            }
        }

        let next_seq = last_valid_seq(path)?.map_or(1, |s| s + 1);
        Ok(LedgerWriter {
            inner: Mutex::new(Inner { file, next_seq }),
            path: path.into(),
            boot_id: boot_id.into(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Assign seq, boot_id, and ts, then append and fsync. The returned line
    /// is what hit the disk.
    pub fn append(&self, mut line: LedgerLine) -> Result<LedgerLine, LedgerError> {
        let io = |source| LedgerError::Io { path: self.path.clone(), source };
        let mut inner = self.inner.lock().expect("ledger writer poisoned");
        line.seq = inner.next_seq;
        line.boot_id = self.boot_id.clone();
        if line.ts == 0 {
            line.ts = now_ms();
        }
        let mut buf = serde_json::to_vec(&line)?;
        buf.push(b'\n');
        inner.file.write_all(&buf).map_err(io)?;
        inner.file.sync_data().map_err(io)?;
        inner.next_seq += 1;
        Ok(line)
    }

    /// The seq the next append will get.
    pub fn next_seq(&self) -> u64 {
        self.inner.lock().expect("ledger writer poisoned").next_seq
    }
}

/// Replay every valid line with seq > cursor (cursor 0 replays everything).
/// Invalid lines are skipped with a warning; they never abort the replay.
pub fn read_after(path: &Path, cursor: u64) -> Result<Vec<LedgerLine>, LedgerError> {
    let io = |source| LedgerError::Io { path: path.into(), source };
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(e)),
    };
    let mut out = Vec::new();
    for (n, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(io)?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LedgerLine>(&line) {
            Ok(l) if l.seq > cursor => out.push(l),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), line = n + 1, %e, "skipping invalid ledger line");
            }
        }
    }
    Ok(out)
}

/// Highest valid seq in the file, if any.
pub fn last_valid_seq(path: &Path) -> Result<Option<u64>, LedgerError> {
    Ok(read_after(path, 0)?.last().map(|l| l.seq))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{Kind, LedgerLine};

    fn msg(subject: &str) -> LedgerLine {
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.ndjson");
        let w = LedgerWriter::open(&path, "boot-1").unwrap();
        let a = w.append(msg("one")).unwrap();
        let b = w.append(msg("two")).unwrap();
        assert_eq!((a.seq, b.seq), (1, 2));
        assert!(a.ts > 0);
        let all = read_after(&path, 0).unwrap();
        assert_eq!(all.len(), 2);
        let after = read_after(&path, 1).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].subject.as_deref(), Some("two"));
    }

    #[test]
    fn seq_continues_across_reopen_with_new_boot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.ndjson");
        {
            let w = LedgerWriter::open(&path, "boot-1").unwrap();
            w.append(msg("one")).unwrap();
        }
        let w = LedgerWriter::open(&path, "boot-2").unwrap();
        let l = w.append(msg("two")).unwrap();
        assert_eq!(l.seq, 2);
        assert_eq!(l.boot_id, "boot-2");
        let all = read_after(&path, 0).unwrap();
        assert_eq!(all[0].boot_id, "boot-1");
        assert_eq!(all[1].boot_id, "boot-2");
    }

    #[test]
    fn torn_tail_is_sealed_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.ndjson");
        {
            let w = LedgerWriter::open(&path, "boot-1").unwrap();
            w.append(msg("one")).unwrap();
        }
        // Simulate a crash mid-append: bytes without a trailing newline.
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(br#"{"seq":2,"boot_id":"boot-1","id":"m-torn","ts":9,"ki"#)
                .unwrap();
        }
        let w = LedgerWriter::open(&path, "boot-2").unwrap();
        let l = w.append(msg("three")).unwrap();
        // The torn line was never valid, so numbering continues after seq 1.
        assert_eq!(l.seq, 2);
        let all = read_after(&path, 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].subject.as_deref(), Some("three"));
    }

    #[test]
    fn missing_file_reads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_after(&dir.path().join("nope.ndjson"), 0).unwrap().is_empty());
    }

    #[test]
    fn invalid_middle_line_does_not_abort() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.ndjson");
        {
            let w = LedgerWriter::open(&path, "b").unwrap();
            w.append(msg("one")).unwrap();
        }
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"not json at all\n").unwrap();
        }
        {
            let w = LedgerWriter::open(&path, "b").unwrap();
            w.append(msg("two")).unwrap();
        }
        let all = read_after(&path, 0).unwrap();
        assert_eq!(all.len(), 2);
    }
}
