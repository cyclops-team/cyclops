//! Data plumbing: the daemon subscription and the startup reconciliation,
//! each on its own task feeding one channel. The event loop only ever
//! receives; it never blocks on the daemon.
//!
//! Zero polling: the subscription pushes, the status request runs once at
//! startup (user-triggered, not timed), and the ledger is read once for
//! backfill. No task here owns a repeating timer.
//!
//! Because that request runs ONCE, everything the register learns after
//! startup arrives on the subscription, including the fact that a pane is
//! gone (`pane-removed`). Re-asking `status` on an interval would answer
//! the same question, and it is the answer this file is not allowed to
//! give: see `cyclops_proto::attention`, "what may feed the register".

use std::path::Path;

use cyclops_proto::{Event, StatusResult};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::StatusSeed;
use crate::entry::Entry;
use crate::input::Key;

/// Everything the event loop can receive.
pub enum UiMsg {
    Key(Key),
    Entry(Box<Entry>),
    Backfill {
        entries: Vec<Entry>,
        max_seq: Option<u64>,
    },
    /// The one-shot startup reconciliation.
    Status(Box<StatusSeed>),
    ConnLost(String),
    Notice(String),
    /// One-shot eye animation step, armed only while the eye is mid-tick.
    EyeTick,
    /// `cyclops theme <name>` moved the selection. Carries nothing: the
    /// event loop's own `ThemeWatch` re-reads the config key and the file
    /// (cyclops-theme's reload rule), and a palette taken off the wire
    /// could show a theme no file on this machine holds.
    ///
    /// Its whole job is to WAKE the loop. The reload already happens
    /// before every frame; on a calm rig there is just no frame until
    /// something arrives.
    ThemeChanged,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Spawn the IO tasks. `home` is the cyclops home (socket + ledger).
pub fn spawn_io(tx: &UnboundedSender<UiMsg>, home: &Path, backfill: usize) {
    let sock = home.join(cyclops_proto::SOCK_NAME);
    tokio::spawn(subscribe_task(tx.clone(), sock.clone()));
    tokio::spawn(seed_task(tx.clone(), sock, home.join("ledger"), backfill));
}

/// The startup reconciliation, in the one order that can be right.
///
/// 1. Ask the daemon where things stand. Its answer names the sessions it
///    watches, where every pane is, and every delivery its fold still
///    counts as needing a human.
/// 2. Replay the tail of THOSE sessions' ledgers and no others. The
///    daemon folds the backlog from the sessions it watches; a stray file
///    from a session nobody watches would put lines on screen that no
///    count owns and no event can ever clear, so the two halves have to
///    agree on which sessions exist, and the daemon's list is the one.
/// 3. Send the seed first, the tail second. `Intake` orders them back:
///    the tail is history, the seed is now, live entries are newer still.
///
/// A daemon that does not answer costs the scope, not the tail: the
/// backfill falls back to every ledger file on disk, and the header
/// already says the connection is gone.
async fn seed_task(
    tx: UnboundedSender<UiMsg>,
    sock: std::path::PathBuf,
    ledger_dir: std::path::PathBuf,
    backfill: usize,
) {
    let seed = status_seed(&sock).await.ok();
    let watched = seed.as_ref().map(|s| s.watched.clone());
    if let Some(seed) = seed {
        if tx.send(UiMsg::Status(Box::new(seed))).is_err() {
            return;
        }
    }
    let _ = tokio::task::spawn_blocking(move || {
        let (entries, max_seq) = read_backfill(&ledger_dir, backfill, watched.as_deref());
        let _ = tx.send(UiMsg::Backfill { entries, max_seq });
    })
    .await;
}

/// The live stream: subscribe, then forward every event as an entry until
/// the connection dies. The ConnLost text is print-ready copy in the
/// CLI's voice: what happened, next step.
async fn subscribe_task(tx: UnboundedSender<UiMsg>, sock: std::path::PathBuf) {
    let text = match subscribe_loop(&tx, &sock).await {
        Ok(()) => broken_words("the connection closed"),
        Err(e) if e.starts_with("cyclops isn't running") => e,
        Err(e) => broken_words(&e),
    };
    let _ = tx.send(UiMsg::ConnLost(text));
}

fn broken_words(cause: &str) -> String {
    format!("lost the connection to cyclops: {cause}. Check that cyclopsd is still running, then retry.")
}

async fn subscribe_loop(tx: &UnboundedSender<UiMsg>, sock: &Path) -> Result<(), String> {
    let stream = UnixStream::connect(sock).await.map_err(connect_words)?;
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    // Hello first (S2). Version mismatch warns nowhere useful in a full
    // screen UI; the protocol is tolerant by design.
    if lines
        .next_line()
        .await
        .map_err(|e| e.to_string())?
        .is_none()
    {
        return Err("the connection closed before hello".into());
    }
    w.write_all(b"{\"id\":1,\"method\":\"events.subscribe\",\"params\":{}}\n")
        .await
        .map_err(|e| e.to_string())?;
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("event").is_none() {
            continue; // the subscribe ack
        }
        let Ok(ev) = serde_json::from_value::<Event>(v) else {
            continue;
        };
        // A theme switch is not a fact about the record, so it does not
        // become a stream entry. It wakes the loop and nothing else.
        let msg = if ev.event == "theme" {
            UiMsg::ThemeChanged
        } else {
            UiMsg::Entry(Box::new(Entry::from_event(&ev, now_ms())))
        };
        if tx.send(msg).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// One status request at startup: the sessions the daemon watches, the
/// label -> pane map behind the focus jump, where every pane stands, and
/// the deliveries still waiting on a human. Failures stay quiet; the
/// subscription owns the connection error surface.
async fn status_seed(sock: &Path) -> Result<StatusSeed, Box<dyn std::error::Error>> {
    let stream = UnixStream::connect(sock).await?;
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    lines.next_line().await?; // hello

    // Any surface that shows the eye must ask for the delivery half; it
    // is half the rule (cyclops_proto::attention). open_deliveries is an
    // additive param: a daemon that predates it ignores it and answers
    // without the field, which decodes as an empty backlog. Tolerant
    // protocol, both directions.
    w.write_all(b"{\"id\":1,\"method\":\"status\",\"params\":{\"open_deliveries\":true}}\n")
        .await?;
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("event").is_some() {
            continue;
        }
        let status: StatusResult = serde_json::from_value(v["result"].clone())?;
        return Ok(StatusSeed::from_status(&status));
    }
    Err("status connection closed early".into())
}

/// The ledger tail: the watched sessions' files under `dir`, mapped onto
/// stream entries, merged by timestamp, last `n` kept. The cursor for
/// dedupe against the live stream is the highest replayed seq, meaningful
/// only while exactly one session file exists (seq is per-file).
///
/// `watched` is the daemon's own session list, and it is the definition of
/// the watched set on both sides: the daemon folds the attention backlog
/// from exactly these sessions. The daemon writes one ledger per watched
/// session at `<home>/ledger/<session>.ndjson` (cyclopsd/src/lib.rs), so
/// the file stem is the session name. None means the daemon did not
/// answer, and the tail falls back to every file on disk.
pub fn read_backfill(
    dir: &Path,
    n: usize,
    watched: Option<&[String]>,
) -> (Vec<Entry>, Option<u64>) {
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for f in rd.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("ndjson") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if watched.is_some_and(|w| !w.iter().any(|s| s == stem)) {
                continue;
            }
            files.push(path);
        }
    }
    files.sort();
    let mut all: Vec<Entry> = Vec::new();
    for path in &files {
        match cyclops_ledger::read_after(path, 0) {
            Ok(lines) => all.extend(lines.iter().filter_map(Entry::from_ledger)),
            Err(_) => continue,
        }
    }
    all.sort_by_key(|e| e.ts);
    let tail = all.split_off(all.len().saturating_sub(n));
    let max_seq = if files.len() == 1 {
        tail.iter().filter_map(|e| e.seq).max()
    } else {
        None
    };
    (tail, max_seq)
}

/// Startup ordering: live entries buffer until the backfill lands, then
/// flush behind it, with ledger-backed duplicates dropped by seq. One
/// watched session dedupes exactly; with several, seq is ambiguous across
/// files and the rare startup-window duplicate is accepted (the ledger
/// itself never duplicates).
///
/// The status seed waits for the backfill too, and lands between the two
/// groups. All three carry a different age and the order is the whole
/// point: the replayed tail is history, the seed is the daemon's answer
/// about now, and a live entry that queued during startup is newer than
/// either. Applying the seed last let a fold taken before a transition
/// re-open an item that transition had just closed.
pub struct Intake {
    backfilled: bool,
    pending: Vec<Entry>,
    pending_status: Option<Box<StatusSeed>>,
    max_seq: Option<u64>,
}

impl Default for Intake {
    fn default() -> Self {
        Self::new()
    }
}

impl Intake {
    pub fn new() -> Intake {
        Intake {
            backfilled: false,
            pending: Vec::new(),
            pending_status: None,
            max_seq: None,
        }
    }

    /// True once the backfill has landed and live entries flow through.
    pub fn is_backfilled(&self) -> bool {
        self.backfilled
    }

    /// A live entry: ready to show now, or empty while buffering.
    pub fn entry(&mut self, e: Entry) -> Vec<Entry> {
        if !self.backfilled {
            self.pending.push(e);
            return Vec::new();
        }
        if self.dup(&e) {
            Vec::new()
        } else {
            vec![e]
        }
    }

    /// The status seed: ready to apply now, or held until the backfill
    /// lands so it reconciles over the replayed tail rather than under it.
    pub fn status(&mut self, seed: Box<StatusSeed>) -> Option<Box<StatusSeed>> {
        if self.backfilled {
            return Some(seed);
        }
        self.pending_status = Some(seed);
        None
    }

    /// The backfill arrived: the three groups, in the order they must be
    /// applied.
    pub fn backfill(&mut self, entries: Vec<Entry>, max_seq: Option<u64>) -> Backfilled {
        self.backfilled = true;
        self.max_seq = max_seq;
        let pending = std::mem::take(&mut self.pending);
        Backfilled {
            replayed: entries,
            seed: self.pending_status.take(),
            live: pending.into_iter().filter(|e| !self.dup(e)).collect(),
        }
    }

    fn dup(&self, e: &Entry) -> bool {
        matches!((e.seq, self.max_seq), (Some(s), Some(m)) if s <= m)
    }
}

/// What the startup window produced, oldest claim first. Apply in field
/// order: `replayed` is history and moves nothing but the screen, `seed`
/// is the daemon's snapshot and replaces the register, `live` are the
/// transitions that happened while the two were loading.
pub struct Backfilled {
    pub replayed: Vec<Entry>,
    pub seed: Option<Box<StatusSeed>>,
    pub live: Vec<Entry>,
}

/// Connection errors in the CLI's words: what happened, next step.
fn connect_words(e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            "cyclops isn't running. Start it with: cyclopsd &".to_string()
        }
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    fn entry(ts: u64, seq: Option<u64>) -> Entry {
        Entry {
            uid: 0,
            ts,
            seq,
            id: None,
            kind: EntryKind::Other {
                event: "x".into(),
                detail: None,
            },
        }
    }

    #[test]
    fn intake_buffers_until_backfill_then_dedupes_by_seq() {
        let mut i = Intake::new();
        // Live entries before the backfill wait.
        assert!(i.entry(entry(5, Some(3))).is_empty());
        assert!(i.entry(entry(6, Some(4))).is_empty());
        assert!(i.entry(entry(7, None)).is_empty());
        // Backfill covers seq 1..=3: the seq-3 pending entry is a dupe,
        // seq 4 and the seq-less one flush behind the backfill.
        let landed = i.backfill(vec![entry(1, Some(1)), entry(3, Some(3))], Some(3));
        assert!(landed.seed.is_none(), "no status seed was waiting");
        let replayed: Vec<Option<u64>> = landed.replayed.iter().map(|e| e.seq).collect();
        assert_eq!(replayed, vec![Some(1), Some(3)]);
        let live: Vec<Option<u64>> = landed.live.iter().map(|e| e.seq).collect();
        assert_eq!(live, vec![Some(4), None]);
        // After the merge, stale live copies still drop; fresh ones pass.
        assert!(i.entry(entry(8, Some(2))).is_empty());
        assert_eq!(i.entry(entry(9, Some(5))).len(), 1);
    }

    #[test]
    fn intake_without_a_cursor_keeps_everything() {
        let mut i = Intake::new();
        assert!(i.entry(entry(5, Some(3))).is_empty());
        let landed = i.backfill(vec![entry(1, Some(9))], None);
        assert_eq!(landed.replayed.len(), 1);
        assert_eq!(landed.live.len(), 1, "no cursor means no dedupe");
    }

    /// The seed lands between the replayed tail and the live entries that
    /// queued behind it. Under the tail, a ledger line older by
    /// construction would overwrite the daemon's answer about now; over
    /// the live entries, a fold taken before a transition would re-open
    /// the item that transition just closed.
    #[test]
    fn the_status_seed_lands_between_history_and_the_live_backlog() {
        let mut i = Intake::new();
        assert!(i.entry(entry(9, None)).is_empty());
        let seed = Box::new(crate::app::StatusSeed::default());
        assert!(i.status(seed).is_none(), "the seed jumped the backfill");
        let landed = i.backfill(vec![entry(1, None)], None);
        assert_eq!(landed.replayed.len(), 1, "history first");
        assert!(landed.seed.is_some(), "the seed never came back");
        assert_eq!(landed.live.len(), 1, "the live backlog goes last");
        // Once the backfill has landed, a late seed applies straight away.
        assert!(i
            .status(Box::new(crate::app::StatusSeed::default()))
            .is_some());
    }

    fn write_session(ledger: &Path, session: &str, subjects: &[&str]) {
        std::fs::create_dir_all(ledger).unwrap();
        let w =
            cyclops_ledger::LedgerWriter::open(&ledger.join(format!("{session}.ndjson")), "b-test")
                .unwrap();
        for (i, subject) in subjects.iter().enumerate() {
            w.append(cyclops_proto::LedgerLine {
                seq: 0,
                boot_id: String::new(),
                id: format!("m-{session}-{i}"),
                ts: 1000 + i as u64,
                kind: cyclops_proto::Kind::Msg,
                from: "codex".into(),
                to: vec!["reviewer".into()],
                subject: Some((*subject).into()),
                body: None,
                reply_to: None,
                deliveries: Vec::new(),
                data: None,
            })
            .unwrap();
        }
    }

    #[test]
    fn backfill_reads_the_tail_of_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger");
        write_session(&ledger, "main", &["s0", "s1", "s2", "s3", "s4"]);
        let watched = ["main".to_string()];
        let (entries, max_seq) = read_backfill(&ledger, 3, Some(&watched));
        assert_eq!(entries.len(), 3);
        assert_eq!(max_seq, Some(5));
        assert!(matches!(&entries[0].kind, EntryKind::Msg { subject, .. } if subject == "s2"));
        // A missing directory reads as an empty backfill, not an error.
        let (entries, max_seq) = read_backfill(&dir.path().join("nope"), 3, Some(&watched));
        assert!(entries.is_empty());
        assert_eq!(max_seq, None);
    }

    /// The daemon folds the attention backlog from the sessions it
    /// watches. Replaying a session it does not watch puts lines on
    /// screen that no count owns and no event can ever clear, so the
    /// watched set has one definition and it is the daemon's.
    #[test]
    fn backfill_replays_only_the_sessions_the_daemon_watches() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger");
        write_session(&ledger, "main", &["watched"]);
        write_session(&ledger, "abandoned", &["stale"]);

        let subjects = |entries: &[Entry]| -> Vec<String> {
            entries
                .iter()
                .filter_map(|e| match &e.kind {
                    EntryKind::Msg { subject, .. } => Some(subject.clone()),
                    _ => None,
                })
                .collect()
        };

        let watched = ["main".to_string()];
        let (entries, max_seq) = read_backfill(&ledger, 10, Some(&watched));
        assert_eq!(subjects(&entries), vec!["watched"]);
        assert_eq!(max_seq, Some(1), "one watched file dedupes by seq");

        // The daemon watching nothing is an answer, not a missing one.
        let (entries, _) = read_backfill(&ledger, 10, Some(&[]));
        assert!(entries.is_empty());

        // No answer at all: show what is on disk rather than an empty
        // screen. The header already says the connection is gone.
        let (entries, max_seq) = read_backfill(&ledger, 10, None);
        assert_eq!(subjects(&entries), vec!["stale", "watched"]);
        assert_eq!(max_seq, None, "per-file seqs collide across files");
    }
}
