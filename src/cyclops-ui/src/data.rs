//! Data plumbing: the daemon subscription and the startup reconciliation,
//! each on its own task feeding one channel. The event loop only ever
//! receives; it never blocks on the daemon.
//!
//! Zero polling: the subscription pushes, the status request and ledger
//! backfill run once, and message snapshots run only after a subscription
//! acknowledgement or a pushed invalidation. No task owns a repeating
//! timer.
//!
//! Because that request runs ONCE, everything the register learns after
//! startup arrives on the subscription, including the fact that a pane is
//! gone (`pane-removed`). Re-asking `status` on an interval would answer
//! the same question, and it is the answer this file is not allowed to
//! give: see `cyclops_proto::attention`, "what may feed the register".
//!
//! This is `cyclops watch`'s own transport: a Unix socket and an NDJSON
//! ledger directory. The ordering discipline that reconciles what it
//! fetches (backfill tail, then the seed, then the live backlog) is
//! backend-neutral and lives in `stream.rs` ([`crate::stream::Intake`]) so
//! a caller with a different transport gets the same guarantee.

use std::path::Path;

use cyclops_proto::{Event, MessagesChangedData, StatusResult};
use cyclops_state::StateRoot;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc::UnboundedSender;

use crate::input::Key;
use crate::messages::RefreshRequest;
use crate::stream::Entry;
use crate::stream::StatusSeed;

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
    /// The event subscription is acknowledged and can no longer miss an
    /// invalidation before the first snapshot starts on its own socket.
    Subscribed,
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
    /// A content-free edge. It wakes the snapshot reducer and never enters
    /// the stream record.
    MessagesChanged(MessagesChangedData),
    /// Session or pane routing changed outside the workspace journal.
    MessagesRouteChanged,
    /// One authenticated messages snapshot, replacing the queue whole.
    Messages {
        request: RefreshRequest,
        snapshot: Box<cyclops_proto::MessagesSnapshotResult>,
    },
    /// A snapshot read failed. The last good one stays on screen.
    MessagesFailed {
        request: RefreshRequest,
        why: String,
    },
    /// A detail read or action came back, under the token it was sent
    /// with. The token is what decides whether it may be applied.
    ActionDone {
        token: crate::action_io::RequestToken,
        outcome: Box<crate::action_io::ActionOutcome>,
    },
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Requests for generation-stamped message snapshots.
pub type MessagesRefresh = UnboundedSender<RefreshRequest>;

/// Spawn the IO tasks. `home` is the cyclops home (socket + ledger).
pub fn spawn_io(tx: &UnboundedSender<UiMsg>, home: &Path, backfill: usize) -> Io {
    let sock = home.join(cyclops_proto::SOCK_NAME);
    spawn_subscribe(tx, home);
    tokio::spawn(seed_task(
        tx.clone(),
        sock.clone(),
        home.join("ledger"),
        backfill,
    ));
    let (refresh_tx, refresh_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(messages_task(tx.clone(), sock.clone(), refresh_rx));
    let (action_tx, action_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(action_task(tx.clone(), sock, action_rx));
    Io {
        refresh: refresh_tx,
        action: action_tx,
    }
}

/// The two channels the event loop drives its own IO with.
pub struct Io {
    pub refresh: MessagesRefresh,
    pub action: UnboundedSender<(
        crate::action_io::RequestToken,
        crate::action_io::ActionRequest,
    )>,
}

/// One detail read or action at a time, off the frame path.
///
/// The loop sends at most one and waits for its answer, so a slow daemon
/// costs an unanswered detail rather than a frozen frame.
async fn action_task(
    tx: UnboundedSender<UiMsg>,
    sock: std::path::PathBuf,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(
        crate::action_io::RequestToken,
        crate::action_io::ActionRequest,
    )>,
) {
    while let Some((token, request)) = rx.recv().await {
        let outcome = crate::action_io::perform(&sock, request).await;
        if tx
            .send(UiMsg::ActionDone {
                token,
                outcome: Box::new(outcome),
            })
            .is_err()
        {
            return;
        }
    }
}

/// One snapshot per request, and never one nobody asked for.
async fn messages_task(
    tx: UnboundedSender<UiMsg>,
    sock: std::path::PathBuf,
    mut refresh: tokio::sync::mpsc::UnboundedReceiver<RefreshRequest>,
) {
    while let Some(request) = refresh.recv().await {
        match messages_snapshot(&sock).await {
            Ok(snapshot) => {
                if tx
                    .send(UiMsg::Messages {
                        request,
                        snapshot: Box::new(snapshot),
                    })
                    .is_err()
                {
                    return;
                }
            }
            // A failed read leaves the last good snapshot on screen and
            // says so. Replacing it with nothing would look like an empty
            // mailbox, which is a different and wrong answer.
            Err(error) => {
                if tx
                    .send(UiMsg::MessagesFailed {
                        request,
                        why: error.to_string(),
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// Bounded on both phases, under the same contract every other socket in
/// this crate uses.
///
/// Unbounded, a daemon that accepts and greets and then never answers
/// parks this task forever. That is worse than a visible failure: the
/// task never reaches `refresh.recv()` again, so later requests pile up
/// unread, and `RefreshGate.in_flight` stays set so `begin` refuses to
/// start another. Every subsequent `messages.changed` edge is then
/// silently dropped while the header still says connected.
///
/// The two phases are split to match [`crate::action_io`], though the
/// distinction costs nothing here: a snapshot is a pure read, so a stall
/// before the write and a stall after it are equally harmless. The last
/// good snapshot stays on screen either way. They are split so there is
/// one timeout contract to reason about rather than two.
async fn messages_snapshot(
    sock: &Path,
) -> Result<cyclops_proto::MessagesSnapshotResult, Box<dyn std::error::Error>> {
    let open = tokio::time::timeout(crate::action_io::OPEN_TIMEOUT, snapshot_open(sock));
    let (mut lines, mut w) = match open.await {
        Ok(opened) => opened?,
        Err(_) => {
            return Err(format!(
                "no connection within {}s",
                crate::action_io::OPEN_TIMEOUT.as_secs()
            )
            .into())
        }
    };
    let answer = tokio::time::timeout(
        crate::action_io::ANSWER_TIMEOUT,
        snapshot_ask(&mut lines, &mut w),
    );
    match answer.await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "no answer within {}s",
            crate::action_io::ANSWER_TIMEOUT.as_secs()
        )
        .into()),
    }
}

type SnapshotLines = tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>;

/// Connect and read the greeting. Nothing has been asked for yet.
async fn snapshot_open(
    sock: &Path,
) -> Result<(SnapshotLines, tokio::net::unix::OwnedWriteHalf), Box<dyn std::error::Error>> {
    let stream = UnixStream::connect(sock).await?;
    let (r, w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    // Same rule as action_io: the greeting has to be one. A socket that
    // closes before greeting, or something else listening on the path,
    // is a failed read and says so, rather than a snapshot request
    // written into whatever is on the other end.
    match lines.next_line().await? {
        Some(line) => {
            serde_json::from_str::<cyclops_proto::Hello>(line.trim())
                .map_err(|_| "not a cyclops daemon")?;
        }
        None => return Err("closed before the hello".into()),
    }
    Ok((lines, w))
}

/// Ask, then read until the answer rather than an event.
async fn snapshot_ask(
    lines: &mut SnapshotLines,
    w: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<cyclops_proto::MessagesSnapshotResult, Box<dyn std::error::Error>> {
    w.write_all(b"{\"id\":1,\"method\":\"messages.snapshot\",\"params\":{}}\n")
        .await?;
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if v.get("event").is_some() {
            continue; // events share the stream
        }
        if let Some(error) = v.get("error") {
            return Err(format!("messages.snapshot: {error}").into());
        }
        return Ok(serde_json::from_value(v["result"].clone())?);
    }
    Err("messages connection closed early".into())
}

/// Start (or restart) the event subscription.
///
/// Called once at startup and again for an explicit reconnect. There is
/// no retry loop and no timer: a daemon that went away is a fact the
/// operator is told about, and asking again is their keystroke. Polling
/// a dead socket forever is the thing this crate does not do.
pub fn spawn_subscribe(tx: &UnboundedSender<UiMsg>, home: &Path) {
    let sock = home.join(cyclops_proto::SOCK_NAME);
    tokio::spawn(subscribe_task(tx.clone(), sock));
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

/// The live stream: acknowledge subscription, then forward records and
/// invalidation edges until the connection dies. The ConnLost text is
/// print-ready copy in the CLI's voice: what happened, next step.
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
    let mut acknowledged = false;
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if !acknowledged {
            if v.get("id").and_then(Value::as_u64) != Some(1) {
                continue;
            }
            if let Some(error) = v.get("error") {
                return Err(format!("events.subscribe: {error}"));
            }
            if v.pointer("/result/subscribed") != Some(&Value::Bool(true)) {
                return Err("events.subscribe returned no acknowledgement".into());
            }
            acknowledged = true;
            if tx.send(UiMsg::Subscribed).is_err() {
                return Ok(());
            }
            continue;
        }
        if v.get("event").is_none() {
            continue;
        }
        let Ok(ev) = serde_json::from_value::<Event>(v) else {
            continue;
        };
        if !forward_event(tx, ev) {
            return Ok(());
        }
    }
    if !acknowledged {
        return Err("the connection closed before the subscribe acknowledgement".into());
    }
    Ok(())
}

/// Forward one typed event. Invalidation edges wake the queue but never
/// become firehose records.
fn forward_event(tx: &UnboundedSender<UiMsg>, ev: Event) -> bool {
    match ev.event.as_str() {
        "messages.changed" => match serde_json::from_value::<MessagesChangedData>(ev.data) {
            Ok(changed) => tx.send(UiMsg::MessagesChanged(changed)).is_ok(),
            Err(_) => true,
        },
        "theme" => tx.send(UiMsg::ThemeChanged).is_ok(),
        "messages.route_changed" => tx.send(UiMsg::MessagesRouteChanged).is_ok(),
        "session" | "pane-removed" => {
            tx.send(UiMsg::MessagesRouteChanged).is_ok()
                && tx
                    .send(UiMsg::Entry(Box::new(Entry::from_event(&ev, now_ms()))))
                    .is_ok()
        }
        _ => tx
            .send(UiMsg::Entry(Box::new(Entry::from_event(&ev, now_ms()))))
            .is_ok(),
    }
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
    let Some(home) = dir.parent() else {
        return (Vec::new(), None);
    };
    let Ok(Some(state_root)) = StateRoot::open_existing(home) else {
        return (Vec::new(), None);
    };
    let Some(directory_name) = dir.file_name() else {
        return (Vec::new(), None);
    };
    let Ok(file_names) = state_root.regular_file_names(Path::new(directory_name)) else {
        return (Vec::new(), None);
    };
    let mut files = Vec::new();
    for file_name in file_names {
        let path = Path::new(&file_name);
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
        files.push(Path::new(directory_name).join(file_name));
    }
    files.sort();
    let mut all: Vec<Entry> = Vec::new();
    for descendant in &files {
        match cyclops_ledger::read_after(&state_root, descendant, 0) {
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

/// Connection errors in the CLI's words: what happened, next step.
///
/// The sentence is cyclops_proto's, not a copy of it. A copy lived here
/// and would have gone on naming `cyclopsd &` after `cyclops start` took
/// over starting the daemon.
fn connect_words(e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            cyclops_proto::NOT_RUNNING.to_string()
        }
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::RefreshGate;
    use crate::stream::EntryKind;
    use std::str::FromStr;
    use std::time::Duration;
    use tokio::net::UnixListener;

    /// A real greeting; the reader rejects anything else.
    fn test_hello() -> String {
        let hello = cyclops_proto::Hello {
            cyclops: "0.0.0-test".into(),
            proto: cyclops_proto::PROTOCOL_VERSION,
            boot_id: "boot-test".into(),
        };
        format!("{}\n", serde_json::to_string(&hello).unwrap())
    }

    fn write_session(ledger: &Path, session: &str, subjects: &[&str]) {
        let home = ledger.parent().expect("ledger has state root");
        let state_root = StateRoot::open_or_create(home).unwrap();
        let descendant = Path::new(ledger.file_name().expect("ledger dir name"))
            .join(format!("{session}.ndjson"));
        let w = cyclops_ledger::LedgerWriter::open(&state_root, &descendant, "b-test").unwrap();
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
        let ledger = std::fs::canonicalize(dir.path()).unwrap().join("ledger");
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
        let ledger = std::fs::canonicalize(dir.path()).unwrap().join("ledger");
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

    fn changed_event(seq: u64) -> Event {
        Event {
            event: "messages.changed".into(),
            data: serde_json::json!({
                "workspace_id": "00000000-0000-0000-0000-000000000001",
                "workspace_seq": seq,
                "changed": ["messages"]
            }),
            seq: Some(seq),
        }
    }

    #[test]
    fn messages_changed_wakes_the_queue_without_becoming_a_stream_entry() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(forward_event(&tx, changed_event(7)));
        match rx.try_recv().unwrap() {
            UiMsg::MessagesChanged(changed) => {
                assert_eq!(changed.workspace_seq, 7);
                assert_eq!(
                    changed.workspace_id,
                    cyclops_proto::WorkspaceId::from_str("00000000-0000-0000-0000-000000000001")
                        .unwrap()
                );
            }
            _ => panic!("messages.changed became a stream record"),
        }
        assert!(rx.try_recv().is_err(), "messages.changed emitted twice");
    }

    #[test]
    fn session_and_pane_events_also_invalidate_route_availability() {
        for event in ["session", "pane-removed"] {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            assert!(forward_event(
                &tx,
                Event {
                    event: event.into(),
                    data: serde_json::json!({"pane_id": "%1"}),
                    seq: None,
                }
            ));
            assert!(matches!(
                rx.try_recv().unwrap(),
                UiMsg::MessagesRouteChanged
            ));
            assert!(matches!(rx.try_recv().unwrap(), UiMsg::Entry(_)));
            assert!(rx.try_recv().is_err());
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(forward_event(
            &tx,
            Event {
                event: "messages.route_changed".into(),
                data: serde_json::json!({}),
                seq: None,
            }
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            UiMsg::MessagesRouteChanged
        ));
        assert!(rx.try_recv().is_err(), "route edge became a stream record");
    }

    /// A daemon that greets and then goes silent must not wedge the
    /// snapshot task.
    ///
    /// Unbounded, `next_line` parks forever: the task never returns to
    /// `refresh.recv()`, so every later request is queued and never read,
    /// and the gate stays in flight so `begin` refuses to make one. The
    /// surface then freezes with the header still saying connected. The
    /// second request here is the real assertion: the first only proves a
    /// failure was reported, the second proves the task survived it.
    ///
    /// The clock runs normally through the socket handshake and is paused
    /// only for the deliberate jump. Starting paused lets tokio's
    /// auto-advance run the clock out during the connect itself, which
    /// times out the wrong phase and tests nothing.
    #[tokio::test]
    async fn a_silent_daemon_fails_one_snapshot_and_keeps_serving() {
        let home = cyclops_proto::scratch::scratch_dir("ui-message-snapshot-timeout");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let sock = home.join(cyclops_proto::SOCK_NAME);
        let listener = UnixListener::bind(&sock).unwrap();

        let mut gate = crate::messages::RefreshGate::new();
        gate.connected();
        let first = gate.begin().expect("a first request is owed");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (refresh_tx, refresh_rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(messages_task(tx, sock.clone(), refresh_rx));

        // Greet, take the request, then say nothing at all. Holding the
        // write half keeps the socket open, which is the case a
        // closed-connection check misses entirely.
        refresh_tx.send(first).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(test_hello().as_bytes()).await.unwrap();
        let mut lines = BufReader::new(read).lines();
        let asked: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(asked["method"], "messages.snapshot");

        // The request is on the wire and no answer is coming. Only now
        // does time matter.
        //
        // A literal, not ANSWER_TIMEOUT + 1. Deriving the jump from the
        // constant makes the test agree with whatever the constant says,
        // so raising the bound would raise the jump too and the test could
        // never fail on it. 15s is chosen to exceed the bound this crate
        // ships; if that bound grows past it, this test is supposed to
        // break and be looked at.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(15)).await;
        tokio::time::resume();

        // Bounded on the real clock so an unbounded read fails here
        // instead of hanging the suite forever.
        let failed = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("no failure inside the bound: the snapshot read is wedged")
            .expect("the task reported nothing");
        match failed {
            UiMsg::MessagesFailed { request, why } => {
                assert_eq!(request, first, "a failure arrived for another request");
                assert!(
                    why.contains("no answer within"),
                    "the wrong phase was bounded: {why}"
                );
            }
            _ => panic!("expected a bounded failure, got another message"),
        }

        // The task is still alive and still reading its channel. Without a
        // bound this explicit reconnect request is never even looked at.
        assert!(gate.finish_failure(first));
        assert!(gate.reconnecting());
        gate.connected();
        let second = gate.begin().expect("the gate freed itself");
        refresh_tx.send(second).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(test_hello().as_bytes()).await.unwrap();
        let mut lines = BufReader::new(read).lines();
        let asked: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            asked["method"], "messages.snapshot",
            "the task never asked again"
        );

        task.abort();
        drop(write);
    }

    /// The subscription socket must acknowledge before the snapshot socket
    /// opens. An edge sent immediately behind the acknowledgement is folded
    /// into the first request rather than falling into a startup gap.
    #[tokio::test]
    async fn subscription_acknowledgement_precedes_the_first_snapshot_socket() {
        let home = cyclops_proto::scratch::scratch_dir("ui-message-subscribe-race");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let sock = home.join(cyclops_proto::SOCK_NAME);
        let listener = UnixListener::bind(&sock).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let subscribe = tokio::spawn(subscribe_task(tx.clone(), sock.clone()));
        let (refresh_tx, refresh_rx) = tokio::sync::mpsc::unbounded_channel();
        let snapshots = tokio::spawn(messages_task(tx, sock.clone(), refresh_rx));

        let (subscription, _) = listener.accept().await.unwrap();
        let (subscription_read, mut subscription_write) = subscription.into_split();
        subscription_write
            .write_all(test_hello().as_bytes())
            .await
            .unwrap();
        let mut subscription_lines = BufReader::new(subscription_read).lines();
        let request: Value =
            serde_json::from_str(&subscription_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["method"], "events.subscribe");

        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err(),
            "snapshot connected before the subscribe acknowledgement"
        );

        subscription_write
            .write_all(
                format!(
                    "{}\n{}\n",
                    serde_json::json!({"id": 1, "result": {"subscribed": true}}),
                    serde_json::to_value(changed_event(1)).unwrap()
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut gate = RefreshGate::new();
        assert!(matches!(rx.recv().await.unwrap(), UiMsg::Subscribed));
        gate.connected();
        match rx.recv().await.unwrap() {
            UiMsg::MessagesChanged(changed) => gate.messages_changed(&changed),
            _ => panic!("the startup invalidation was not typed"),
        }
        let refresh = gate.begin().expect("the acknowledgement owes a snapshot");
        refresh_tx.send(refresh).unwrap();

        let (snapshot, _) = listener.accept().await.unwrap();
        let (snapshot_read, mut snapshot_write) = snapshot.into_split();
        snapshot_write
            .write_all(test_hello().as_bytes())
            .await
            .unwrap();
        let mut snapshot_lines = BufReader::new(snapshot_read).lines();
        let request: Value =
            serde_json::from_str(&snapshot_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["method"], "messages.snapshot");
        snapshot_write
            .write_all(
                format!(
                    "{}\n",
                    serde_json::json!({
                        "id": 1,
                        "result": {
                            "workspace_id": "00000000-0000-0000-0000-000000000001",
                            "workspace_seq": 1,
                            "counts": {
                                "visible_messages": 0,
                                "returned_messages": 0,
                                "inbox_messages": 0,
                                "outbound_messages": 0,
                                "work_messages": 0,
                                "active_messages": 0,
                                "settled_messages": 0,
                                "pending_entries": 0,
                                "claimed_entries": 0,
                                "open_attention_entries": 0
                            },
                            "rows": []
                        }
                    })
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        match rx.recv().await.unwrap() {
            UiMsg::Messages { request, snapshot } => {
                assert_eq!(request, refresh);
                assert!(gate.finish_snapshot(request, &snapshot));
            }
            _ => panic!("the snapshot response was not delivered"),
        }

        subscribe.abort();
        snapshots.abort();
        let _ = std::fs::remove_dir_all(home);
    }
}
