//! Data plumbing: the daemon subscription owns each connection epoch's
//! reconciliation, while action and message workers feed separate bounded
//! result lanes. The event loop only ever receives; it never blocks on the
//! daemon.
//!
//! Zero polling: the subscription pushes, status and daemon backfill run once
//! per acknowledged connection, and message snapshots run only after that
//! acknowledgement or a pushed invalidation. No task owns a repeating timer.
//!
//! Because that request runs ONCE, everything the register learns after
//! startup arrives on the subscription, including the fact that a pane is
//! gone (`pane-removed`). Re-asking `status` on an interval would answer
//! the same question, and it is the answer this file is not allowed to
//! give: see `cyclops_proto::attention`, "what may feed the register".
//!
//! This is `cyclops watch`'s concrete socket adapter. Journal ownership stays
//! in the daemon; the UI receives a bounded, body-free projection. The
//! ordering discipline that reconciles what it
//! fetches (backfill tail, then the seed, then the live backlog) is
//! backend-neutral and lives in `stream.rs` ([`crate::stream::Intake`]) so
//! a caller with a different transport gets the same guarantee.

use std::path::Path;

use cyclops_proto::{Event, MessagesChangedData, StatusResult, StreamBackfillResult};
use tokio::sync::mpsc;

use crate::input::Key;
use crate::messages::RefreshRequest;
use crate::stream::Entry;
use crate::stream::StatusSeed;
use cyclops_client::{AsyncClient, ClientError};

type UiError = Box<dyn std::error::Error + Send + Sync>;

/// Everything the event loop can receive.
pub enum UiMsg {
    Key(Key),
    Entry(Box<Entry>),
    /// One daemon-owned, bounded replacement for the stream projection.
    /// It lands atomically before live events from the same subscription.
    StreamProjection(Box<StreamProjection>),
    /// The event subscription is acknowledged and can no longer miss an
    /// invalidation before the first snapshot starts on its own socket.
    Subscribed,
    ConnLost(String),
    Notice(String),
    /// Exact client and daemon build identity from the socket greeting.
    BuildHealth(crate::health::BuildHealth),
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
    /// One bounded cursor page of durable message arrivals.
    MessagesFollow {
        request: crate::messages::FollowRequest,
        page: Box<cyclops_proto::MessagesFollowResult>,
    },
    MessagesFollowFailed {
        request: crate::messages::FollowRequest,
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

/// The event loop starts at most one request of each kind. A single slot
/// therefore bounds memory without adding latency or discarding work.
const REQUEST_CAPACITY: usize = 1;

/// The three result lanes keep ordered events from delaying a snapshot or
/// an operator action. Each sender applies backpressure when its lane is
/// full; no result is dropped or silently coalesced.
#[derive(Clone)]
pub struct UiSinks {
    pub events: mpsc::Sender<UiMsg>,
    pub snapshots: mpsc::Sender<UiMsg>,
    pub actions: mpsc::Sender<UiMsg>,
}

/// Requests for generation-stamped message snapshots.
pub type MessagesRefresh = mpsc::Sender<RefreshRequest>;

/// Concrete launcher-owned pane focus effect. Presentation emits a target;
/// the terminal adapter decides how that target is reached.
pub type FocusPane = fn(&str) -> Result<(), String>;

/// Spawn the IO tasks. `home` supplies the daemon socket location.
pub fn spawn_io(sinks: &UiSinks, home: &Path, backfill: usize, focus: Option<FocusPane>) -> Io {
    let sock = home.join(cyclops_proto::SOCK_NAME);
    let (reconnect_tx, reconnect_rx) = mpsc::channel(REQUEST_CAPACITY);
    reconnect_tx
        .try_send(())
        .expect("new subscription controller has one free slot");
    tokio::spawn(subscription_task(
        sinks.events.clone(),
        sock.clone(),
        reconnect_rx,
        backfill,
    ));
    let (refresh_tx, refresh_rx) = mpsc::channel(REQUEST_CAPACITY);
    tokio::spawn(messages_task(
        sinks.snapshots.clone(),
        sock.clone(),
        refresh_rx,
    ));
    let (follow_tx, follow_rx) = mpsc::channel(REQUEST_CAPACITY);
    tokio::spawn(follow_task(
        sinks.snapshots.clone(),
        sock.clone(),
        follow_rx,
    ));
    let (action_tx, action_rx) = mpsc::channel(REQUEST_CAPACITY);
    tokio::spawn(action_task(sinks.actions.clone(), sock, action_rx));
    let (focus_tx, focus_rx) = mpsc::channel(REQUEST_CAPACITY);
    tokio::spawn(focus_task(sinks.actions.clone(), focus_rx, focus));
    Io {
        reconnect: reconnect_tx,
        refresh: refresh_tx,
        follow: follow_tx,
        action: action_tx,
        focus: focus_tx,
    }
}

/// Bounded command handles for the event loop's serial IO workers.
pub struct Io {
    /// Coalesced requests to the one task that owns the event subscription.
    pub reconnect: mpsc::Sender<()>,
    pub refresh: MessagesRefresh,
    pub follow: mpsc::Sender<crate::messages::FollowRequest>,
    pub action: mpsc::Sender<(
        crate::action_io::RequestToken,
        crate::action_io::ActionRequest,
    )>,
    /// One active tmux focus call and at most one queued request.
    pub focus: mpsc::Sender<String>,
}

async fn focus_task(
    tx: mpsc::Sender<UiMsg>,
    mut rx: mpsc::Receiver<String>,
    focus: Option<FocusPane>,
) {
    while let Some(pane) = rx.recv().await {
        let target = pane.clone();
        let result = match focus {
            Some(focus) => tokio::task::spawn_blocking(move || focus(&target)).await,
            None => {
                if tx
                    .send(UiMsg::Notice(format!(
                        "can't jump to {pane}: this launcher has no pane-focus adapter"
                    )))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };
        let notice = match result {
            Ok(Ok(())) => continue,
            Ok(Err(error)) => format!("can't jump to {pane}: {error}"),
            Err(error) => format!("can't jump to {pane}: focus worker failed: {error}"),
        };
        if tx.send(UiMsg::Notice(notice)).await.is_err() {
            return;
        }
    }
}

async fn follow_task(
    tx: mpsc::Sender<UiMsg>,
    sock: std::path::PathBuf,
    mut follow: mpsc::Receiver<crate::messages::FollowRequest>,
) {
    while let Some(request) = follow.recv().await {
        match messages_follow(&sock, request).await {
            Ok(page) => {
                if tx
                    .send(UiMsg::MessagesFollow {
                        request,
                        page: Box::new(page),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                if tx
                    .send(UiMsg::MessagesFollowFailed {
                        request,
                        why: error.to_string(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// One detail read or action at a time, off the frame path.
///
/// The loop sends at most one and waits for its answer, so a slow daemon
/// costs an unanswered detail rather than a frozen frame.
async fn action_task(
    tx: mpsc::Sender<UiMsg>,
    sock: std::path::PathBuf,
    mut rx: mpsc::Receiver<(
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
            .await
            .is_err()
        {
            return;
        }
    }
}

/// One snapshot per request, and never one nobody asked for.
async fn messages_task(
    tx: mpsc::Sender<UiMsg>,
    sock: std::path::PathBuf,
    mut refresh: mpsc::Receiver<RefreshRequest>,
) {
    while let Some(request) = refresh.recv().await {
        match messages_snapshot(&sock).await {
            Ok(snapshot) => {
                if tx
                    .send(UiMsg::Messages {
                        request,
                        snapshot: Box::new(snapshot),
                    })
                    .await
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
                    .await
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
async fn messages_snapshot(sock: &Path) -> Result<cyclops_proto::MessagesSnapshotResult, UiError> {
    let mut client = open_client(sock).await?;
    let result = client
        .request(
            "messages.snapshot",
            serde_json::json!({}),
            crate::action_io::ANSWER_TIMEOUT,
        )
        .await?;
    let snapshot: cyclops_proto::MessagesSnapshotResult = serde_json::from_value(result)?;
    let queue_rows = snapshot.rows.iter().fold(0usize, |count, row| {
        count.saturating_add(row.recipients.len())
    });
    if snapshot.rows.len() > crate::stream::RING_CAP || queue_rows > crate::stream::RING_CAP {
        return Err(format!(
            "messages.snapshot exceeds the {}-item UI limit",
            crate::stream::RING_CAP
        )
        .into());
    }
    Ok(snapshot)
}

async fn messages_follow(
    sock: &Path,
    request: crate::messages::FollowRequest,
) -> Result<cyclops_proto::MessagesFollowResult, UiError> {
    let mut client = open_client(sock).await?;
    let result = client
        .request(
            "messages.follow",
            serde_json::json!({
                "after_seq": request.after_seq(),
                "limit": request.limit(),
            }),
            crate::action_io::ANSWER_TIMEOUT,
        )
        .await?;
    let page: cyclops_proto::MessagesFollowResult = serde_json::from_value(result)?;
    if page.rows.len() > request.limit() as usize {
        return Err(format!(
            "messages.follow returned {} rows for a {}-row request",
            page.rows.len(),
            request.limit()
        )
        .into());
    }
    Ok(page)
}

async fn open_client(sock: &Path) -> Result<AsyncClient, ClientError> {
    AsyncClient::connect(sock, crate::action_io::OPEN_TIMEOUT).await
}

/// One connection epoch's reconciliation, in the one order that can be right.
///
/// 1. Ask the daemon where things stand. Its answer names the sessions it
///    watches, where every pane is, and every delivery its fold still
///    counts as needing a human.
/// 2. Replay the tail of THOSE sessions' ledgers and no others. The
///    daemon folds the backlog from the sessions it watches; a stray file
///    from a session nobody watches would put lines on screen that no
///    count owns and no event can ever clear, so the two halves have to
///    agree on which sessions exist, and the daemon's list is the one.
/// 3. Return both as one replacement projection. The consumer applies the
///    tail as history, the seed as now, then live entries from this connection.
///
/// A daemon that does not answer leaves both projections unavailable. The
/// surface reports that gap and points to the durable history command; it
/// never guesses journal paths or reads storage behind the daemon.
async fn stream_projection(sock: &Path, backfill: usize) -> StreamProjection {
    let mut warnings = Vec::new();
    let seed = match status_seed(sock).await {
        Ok(seed) => Some(seed),
        Err(error) => {
            warnings.push(format!(
                "status snapshot unavailable; state may be incomplete: {error}"
            ));
            None
        }
    };
    let report = match stream_backfill(sock, backfill).await {
        Ok(result) => project_backfill(result),
        Err(error) => BackfillReport {
            entries: Vec::new(),
            max_seq: None,
            warning: Some(format!(
                "backfill unavailable; stream history has a gap: {error}. Use cyclops history for the durable record"
            )),
        },
    };
    if let Some(warning) = report.warning {
        warnings.push(warning);
    }
    StreamProjection {
        seed: seed.map(Box::new),
        entries: report.entries,
        max_seq: report.max_seq,
        warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
    }
}

/// Everything needed to replace the reusable stream model after an
/// acknowledged subscription. Missing pieces remain explicit in `warning`.
pub struct StreamProjection {
    pub seed: Option<Box<StatusSeed>>,
    pub entries: Vec<Entry>,
    pub max_seq: Option<u64>,
    pub warning: Option<String>,
}

/// The live stream: acknowledge subscription, then forward records and
/// invalidation edges until the connection dies. The ConnLost text is
/// print-ready copy in the CLI's voice: what happened, next step.
async fn subscription_task(
    tx: mpsc::Sender<UiMsg>,
    sock: std::path::PathBuf,
    mut reconnect: mpsc::Receiver<()>,
    backfill: usize,
) {
    // This is the only owner of an event socket. A capacity-one command
    // lane coalesces repeated R presses without creating overlapping
    // generations or an unbounded queue of future retries.
    let mut projection_landed = false;
    while reconnect.recv().await.is_some() {
        while reconnect.try_recv().is_ok() {}
        let (text, this_projection_landed) = match subscribe_loop(&tx, &sock, backfill).await {
            Ok(landed) => (
                broken_words("the connection closed; the live stream may have a gap"),
                landed,
            ),
            Err(failure) if failure.cause.starts_with("cyclops isn't running") => {
                projection_landed |= failure.projection_landed;
                if !projection_landed
                    && tx
                        .send(UiMsg::StreamProjection(Box::new(StreamProjection {
                            seed: None,
                            entries: Vec::new(),
                            max_seq: None,
                            warning: Some(format!(
                                "stream snapshot unavailable; state may be incomplete: {}",
                                failure.cause
                            )),
                        })))
                        .await
                        .is_err()
                {
                    return;
                }
                (failure.cause, failure.projection_landed)
            }
            Err(failure) => {
                projection_landed |= failure.projection_landed;
                if !projection_landed
                    && tx
                        .send(UiMsg::StreamProjection(Box::new(StreamProjection {
                            seed: None,
                            entries: Vec::new(),
                            max_seq: None,
                            warning: Some(format!(
                                "stream snapshot unavailable; state may be incomplete: {}",
                                failure.cause
                            )),
                        })))
                        .await
                        .is_err()
                {
                    return;
                }
                (
                    broken_words(&format!(
                        "{}; the live stream may have a gap",
                        failure.cause
                    )),
                    failure.projection_landed,
                )
            }
        };
        projection_landed |= this_projection_landed;
        if tx.send(UiMsg::ConnLost(text)).await.is_err() {
            return;
        }
    }
}

fn broken_words(cause: &str) -> String {
    format!("lost the connection to cyclops: {cause}. Check that cyclopsd is still running, then retry.")
}

async fn subscribe_loop(
    tx: &mpsc::Sender<UiMsg>,
    sock: &Path,
    backfill: usize,
) -> Result<bool, SubscriptionFailure> {
    let mut client = open_client(sock)
        .await
        .map_err(|error| SubscriptionFailure {
            cause: client_words(error),
            projection_landed: false,
        })?;
    // Hello first (S2). The protocol remains tolerant, but build drift is
    // persistent UI health rather than a warning lost before raw mode starts.
    if tx
        .send(UiMsg::BuildHealth(crate::health::BuildHealth::from_hello(
            client.hello(),
        )))
        .await
        .is_err()
    {
        return Ok(false);
    }
    client
        .subscribe(serde_json::json!({}), crate::action_io::ANSWER_TIMEOUT)
        .await
        .map_err(|error| SubscriptionFailure {
            cause: client_words(error),
            projection_landed: false,
        })?;
    // The subscription is already established, so changes that happen while
    // these bounded snapshot reads run wait on this socket. Replacing the
    // projection before reading those events closes both startup and reconnect
    // gaps without treating the ephemeral event stream as retained truth.
    let projection = stream_projection(sock, backfill).await;
    if tx
        .send(UiMsg::StreamProjection(Box::new(projection)))
        .await
        .is_err()
    {
        return Ok(true);
    }
    if tx.send(UiMsg::Subscribed).await.is_err() {
        return Ok(true);
    }
    loop {
        let frame = client
            .next_event()
            .await
            .map_err(|error| SubscriptionFailure {
                cause: client_words(error),
                projection_landed: true,
            })?;
        if !forward_event(tx, frame.event)
            .await
            .map_err(|cause| SubscriptionFailure {
                cause,
                projection_landed: true,
            })?
        {
            return Ok(true);
        }
    }
}

struct SubscriptionFailure {
    cause: String,
    projection_landed: bool,
}

/// Forward one typed event. Invalidation edges wake the queue but never
/// become firehose records.
async fn forward_event(tx: &mpsc::Sender<UiMsg>, ev: Event) -> Result<bool, String> {
    match ev.event.as_str() {
        "messages.changed" => match serde_json::from_value::<MessagesChangedData>(ev.data) {
            Ok(changed) => Ok(tx.send(UiMsg::MessagesChanged(changed)).await.is_ok()),
            Err(error) => Err(format!("malformed messages.changed event: {error}")),
        },
        "theme" => Ok(tx.send(UiMsg::ThemeChanged).await.is_ok()),
        "messages.route_changed" => Ok(tx.send(UiMsg::MessagesRouteChanged).await.is_ok()),
        "session" | "pane-removed" => Ok(tx.send(UiMsg::MessagesRouteChanged).await.is_ok()
            && tx
                .send(UiMsg::Entry(Box::new(Entry::from_event(&ev, now_ms()))))
                .await
                .is_ok()),
        _ => Ok(tx
            .send(UiMsg::Entry(Box::new(Entry::from_event(&ev, now_ms()))))
            .await
            .is_ok()),
    }
}

/// One status request at startup: the sessions the daemon watches, the
/// label -> pane map behind the focus jump, where every pane stands, and
/// the deliveries still waiting on a human. A failed or malformed answer is
/// visible because it changes the scope and freshness of the read model.
async fn status_seed(sock: &Path) -> Result<StatusSeed, UiError> {
    let mut client = open_client(sock).await?;

    // Any surface that shows the eye must ask for the delivery half; it
    // is half the rule (cyclops_proto::attention). open_deliveries is an
    // additive param: a daemon that predates it ignores it and answers
    // without the field, which decodes as an empty backlog. Tolerant
    // protocol, both directions.
    let result = client
        .request(
            "status",
            serde_json::json!({"open_deliveries": true}),
            crate::action_io::ANSWER_TIMEOUT,
        )
        .await
        .map_err(|error| -> UiError { Box::new(error) })?;
    let status: StatusResult = serde_json::from_value(result)?;
    let status_items = status
        .sessions
        .iter()
        .fold(status.sessions.len(), |count, session| {
            count.saturating_add(session.panes.len())
        })
        .saturating_add(status.mailbox_routes.len())
        .saturating_add(status.open_deliveries.len())
        .saturating_add(status.mailbox_attention.len())
        .saturating_add(status.diagnostics.len())
        .saturating_add(status.blocked_notifications.len());
    if status_items > crate::stream::RING_CAP {
        return Err(format!(
            "status exceeds the {}-item UI limit",
            crate::stream::RING_CAP
        )
        .into());
    }
    Ok(StatusSeed::from_status(&status))
}

/// A bounded daemon projection plus any explicitly reported loss.
#[derive(Debug)]
pub struct BackfillReport {
    /// Entries retained in timestamp order.
    pub entries: Vec<Entry>,
    /// Highest retained sequence when exactly one file supplied the tail.
    pub max_seq: Option<u64>,
    /// Visible gap text when requested history could not be represented whole.
    pub warning: Option<String>,
}

/// Convert the daemon-owned read projection into renderer-neutral entries.
pub fn project_backfill(result: StreamBackfillResult) -> BackfillReport {
    let entries = result.lines.iter().filter_map(Entry::from_ledger).collect();
    let warning = result.gap.filter(|gap| !gap.is_empty()).map(|gap| {
        let mut facts = Vec::new();
        if gap.unreadable_sources > 0 {
            facts.push(format!("{} unreadable sources", gap.unreadable_sources));
        }
        if gap.omitted_rows > 0 {
            facts.push(format!("{} rows beyond the retained limits", gap.omitted_rows));
        }
        format!(
            "backfill incomplete; stream history has a gap: {}. Use cyclops history for the durable record",
            facts.join(", ")
        )
    });
    BackfillReport {
        entries,
        max_seq: result.max_seq,
        warning,
    }
}

async fn stream_backfill(sock: &Path, limit: usize) -> Result<StreamBackfillResult, UiError> {
    let mut client = open_client(sock).await?;
    let limit = u32::try_from(limit).unwrap_or(u32::MAX);
    let result = client
        .request(
            "events.backfill",
            serde_json::json!({"limit": limit}),
            crate::action_io::ANSWER_TIMEOUT,
        )
        .await
        .map_err(|error| -> UiError { Box::new(error) })?;
    serde_json::from_value(result).map_err(|error| -> UiError { Box::new(error) })
}

/// Connection errors in the CLI's words: what happened, next step.
///
/// The sentence is cyclops_proto's, not a copy of it. A copy lived here
/// and would have gone on naming `cyclopsd &` after `cyclops start` took
/// over starting the daemon.
fn client_words(error: ClientError) -> String {
    match error {
        ClientError::NotRunning(_) => cyclops_proto::NOT_RUNNING.to_string(),
        other => other.cause(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::RefreshGate;
    use crate::stream::EntryKind;
    use serde_json::Value;
    use std::str::FromStr;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// A real greeting; the reader rejects anything else.
    fn test_hello() -> String {
        let hello = cyclops_proto::Hello {
            cyclops: "0.0.0-test".into(),
            build: None,
            daemon_process: None,
            daemon_executable: None,
            proto: cyclops_proto::PROTOCOL_VERSION,
            boot_id: "boot-test".into(),
        };
        format!("{}\n", serde_json::to_string(&hello).unwrap())
    }

    /// Answer one projection request with an explicit unsupported error. The
    /// subscription still becomes usable, but its replacement snapshot must
    /// carry the gap instead of falling back to local storage.
    async fn reject_projection_request(listener: &UnixListener, method: &str) {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(test_hello().as_bytes()).await.unwrap();
        let mut lines = BufReader::new(read).lines();
        let request: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["method"], method);
        write
            .write_all(
                format!(
                    "{}\n",
                    serde_json::json!({
                        "id": request["id"],
                        "error": {"code": -32601, "message": "unsupported in fixture"}
                    })
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    }

    async fn reject_projection(listener: &UnixListener) {
        reject_projection_request(listener, "status").await;
        reject_projection_request(listener, "events.backfill").await;
    }

    #[test]
    fn daemon_backfill_projects_rows_and_preserves_explicit_gaps() {
        let result = StreamBackfillResult {
            lines: vec![cyclops_proto::LedgerLine {
                seq: 7,
                boot_id: "b-test".into(),
                id: "m-one".into(),
                ts: 1_000,
                kind: cyclops_proto::Kind::Msg,
                from: "codex".into(),
                to: vec!["reviewer".into()],
                subject: Some("focused review".into()),
                body: None,
                reply_to: None,
                deliveries: Vec::new(),
                data: None,
            }],
            max_seq: Some(7),
            gap: Some(cyclops_proto::StreamBackfillGap {
                unreadable_sources: 1,
                omitted_rows: 2,
            }),
        };

        let report = project_backfill(result);
        assert_eq!(report.max_seq, Some(7));
        assert!(
            matches!(&report.entries[0].kind, EntryKind::Msg { subject, .. } if subject == "focused review")
        );
        let warning = report.warning.expect("the daemon gap stays visible");
        assert!(warning.contains("1 unreadable sources"), "{warning}");
        assert!(warning.contains("2 rows beyond"), "{warning}");
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

    #[tokio::test]
    async fn messages_changed_wakes_the_queue_without_becoming_a_stream_entry() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        assert!(forward_event(&tx, changed_event(7)).await.unwrap());
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

    #[tokio::test]
    async fn session_and_pane_events_also_invalidate_route_availability() {
        for event in ["session", "pane-removed"] {
            let (tx, mut rx) = tokio::sync::mpsc::channel(4);
            assert!(forward_event(
                &tx,
                Event {
                    event: event.into(),
                    data: serde_json::json!({"pane_id": "%1"}),
                    seq: None,
                }
            )
            .await
            .unwrap());
            assert!(matches!(
                rx.try_recv().unwrap(),
                UiMsg::MessagesRouteChanged
            ));
            assert!(matches!(rx.try_recv().unwrap(), UiMsg::Entry(_)));
            assert!(rx.try_recv().is_err());
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        assert!(forward_event(
            &tx,
            Event {
                event: "messages.route_changed".into(),
                data: serde_json::json!({}),
                seq: None,
            }
        )
        .await
        .unwrap());
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

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let (refresh_tx, refresh_rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(messages_task(tx, sock.clone(), refresh_rx));

        // Greet, take the request, then say nothing at all. Holding the
        // write half keeps the socket open, which is the case a
        // closed-connection check misses entirely.
        refresh_tx.send(first).await.unwrap();
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
        refresh_tx.send(second).await.unwrap();
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

    /// The subscription socket must acknowledge before any snapshot socket
    /// opens. An edge sent immediately behind the acknowledgement waits while
    /// the bounded replacement is loaded, then lands after it instead of
    /// falling into a startup gap.
    #[tokio::test]
    async fn subscription_acknowledgement_precedes_the_first_snapshot_socket() {
        let home = cyclops_proto::scratch::scratch_dir("ui-message-subscribe-race");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let sock = home.join(cyclops_proto::SOCK_NAME);
        let listener = UnixListener::bind(&sock).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let (reconnect_tx, reconnect_rx) = tokio::sync::mpsc::channel(1);
        reconnect_tx.try_send(()).unwrap();
        let subscribe = tokio::spawn(subscription_task(
            tx.clone(),
            sock.clone(),
            reconnect_rx,
            200,
        ));
        let (refresh_tx, refresh_rx) = tokio::sync::mpsc::channel(1);
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

        reject_projection(&listener).await;

        let mut gate = RefreshGate::new();
        assert!(matches!(
            rx.recv().await.unwrap(),
            UiMsg::BuildHealth(crate::health::BuildHealth::LegacyDaemon { .. })
        ));
        match rx.recv().await.unwrap() {
            UiMsg::StreamProjection(projection) => {
                assert!(projection.seed.is_none());
                assert!(projection.entries.is_empty());
                assert!(
                    projection
                        .warning
                        .as_deref()
                        .is_some_and(|warning| warning.contains("unsupported in fixture")),
                    "the missing daemon projection was not explicit"
                );
            }
            _ => panic!("subscription did not replace its stream projection"),
        }
        assert!(matches!(rx.recv().await.unwrap(), UiMsg::Subscribed));
        gate.connected();
        match rx.recv().await.unwrap() {
            UiMsg::MessagesChanged(changed) => gate.messages_changed(&changed),
            _ => panic!("the startup invalidation was not typed"),
        };
        let refresh = gate.begin().expect("the acknowledgement owes a snapshot");
        refresh_tx.send(refresh).await.unwrap();

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

    #[tokio::test]
    async fn malformed_live_frame_waits_for_one_explicit_reconnect() {
        let home = cyclops_proto::scratch::scratch_dir("ui-subscription-controller-gap");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let sock = home.join(cyclops_proto::SOCK_NAME);
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (reconnect_tx, reconnect_rx) = tokio::sync::mpsc::channel(1);
        reconnect_tx.try_send(()).unwrap();
        let controller = tokio::spawn(subscription_task(tx, sock.clone(), reconnect_rx, 200));

        let (first, _) = listener.accept().await.unwrap();
        let (first_read, mut first_write) = first.into_split();
        first_write
            .write_all(test_hello().as_bytes())
            .await
            .unwrap();
        let mut first_lines = BufReader::new(first_read).lines();
        let request: Value =
            serde_json::from_str(&first_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["method"], "events.subscribe");
        first_write
            .write_all(b"{\"id\":1,\"result\":{\"subscribed\":true}}\n")
            .await
            .unwrap();
        reject_projection(&listener).await;
        assert!(matches!(rx.recv().await.unwrap(), UiMsg::BuildHealth(_)));
        assert!(matches!(
            rx.recv().await.unwrap(),
            UiMsg::StreamProjection(_)
        ));
        assert!(matches!(rx.recv().await.unwrap(), UiMsg::Subscribed));

        first_write.write_all(b"{malformed}\n").await.unwrap();
        let lost = rx.recv().await.unwrap();
        match lost {
            UiMsg::ConnLost(why) => {
                assert!(why.contains("malformed event frame"), "{why}");
                assert!(why.contains("live stream may have a gap"), "{why}");
            }
            _ => panic!("malformed live input did not expose a connection gap"),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err(),
            "the controller retried without an operator request"
        );

        reconnect_tx.try_send(()).unwrap();
        assert!(
            reconnect_tx.try_send(()).is_err(),
            "reconnects did not coalesce"
        );
        let (second, _) = listener.accept().await.unwrap();
        let (second_read, mut second_write) = second.into_split();
        second_write
            .write_all(test_hello().as_bytes())
            .await
            .unwrap();
        let mut second_lines = BufReader::new(second_read).lines();
        let request: Value =
            serde_json::from_str(&second_lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["method"], "events.subscribe");
        second_write
            .write_all(b"{\"id\":1,\"result\":{\"subscribed\":true}}\n")
            .await
            .unwrap();
        reject_projection(&listener).await;
        assert!(matches!(rx.recv().await.unwrap(), UiMsg::BuildHealth(_)));
        assert!(matches!(
            rx.recv().await.unwrap(),
            UiMsg::StreamProjection(_)
        ));
        assert!(matches!(rx.recv().await.unwrap(), UiMsg::Subscribed));

        drop(second_write);
        assert!(matches!(rx.recv().await.unwrap(), UiMsg::ConnLost(_)));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err(),
            "a coalesced reconnect became a later automatic retry"
        );

        controller.abort();
        let _ = std::fs::remove_dir_all(home);
    }
}
