//! Read-side queries over the authoritative workspace message journal.
//!
//! Pre-upgrade session ledgers remain readable as compatibility sources.
//! Workspace records are read first and duplicate message identifiers are
//! returned once. New messages are never copied into a session ledger.
//!
//! The read model folds each message's delivery chain (the kind=state lines
//! appended after the msg line) back into the msg line's `deliveries`, so a
//! returned line carries the latest recorded state per recipient: one msg
//! fact, N current badges. Folding happens at read time; nothing on disk is
//! rewritten.
//!
//! Reader choice: cyclops-ledger's `read_after` full scan. A journal with
//! 10,000 lines parses in single-digit milliseconds on this
//! machine, so no indexed reader was added to that crate; the additive
//! index stays a measured-need option, not a speculative one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use cyclops_proto::{
    HistoryParams, HistoryResult, Kind, LedgerLine, MessageMetadata, OpenDelivery, RecipientKey,
    ThreadResult, WireError,
};
use serde_json::Value;
use tracing::warn;

use crate::identity;
use crate::mailbox::MailboxIdentity;
use crate::server::sender_panes;
use crate::Inner;

/// Peer credentials as the connection captured them (uid, pid). None means
/// the kernel could not report them; "me" resolution fails closed on it.
pub(crate) type Peer = Option<crate::identity::PeerConn>;

fn wire_err(code: &str, msg: impl Into<String>) -> WireError {
    WireError {
        code: code.to_string(),
        message: msg.into(),
        data: None,
    }
}

// ---------------------------------------------------------------------------
// Socket entry points
// ---------------------------------------------------------------------------

/// msg.history: filter the folded message record. `--with X` means from or
/// to X; `--from`/`--to` filter one direction each; the name "me" in any
/// filter resolves through the caller's identity envelope. Newest last.
///
/// Paging: with ONE watched session the u64 `cursor` pages on the file's
/// seq, byte-identical to the shipped behavior. With several watched
/// sessions per-file seqs collide, so the u64 cursor is refused and paging
/// runs on the opaque composite `cursor2` instead (a per-session consumed
/// position), which never skips a message however the files interleave.
pub(crate) fn msg_history(
    inner: &Arc<Inner>,
    params: HistoryParams,
    cursor2: Option<String>,
    peer: Peer,
) -> Result<Value, WireError> {
    if params.with.is_some() && (params.from.is_some() || params.to.is_some()) {
        return Err(wire_err(
            "bad_request",
            "pick one filter shape: with, or from/to",
        ));
    }
    let caller = history_caller(inner, peer)?;
    let with = resolve_filter(inner, peer, params.with, caller.as_ref())?;
    let from = resolve_filter(inner, peer, params.from, caller.as_ref())?;
    let to = resolve_filter(inner, peer, params.to, caller.as_ref())?;
    let limit = params.limit as usize;

    let (files, names, workspace_file) = history_sources(inner);
    let metadata = message_metadata(&files);
    let matches = |line: &LedgerLine| {
        let metadata = metadata.get(&line.id);
        line_visible_to(metadata, caller.as_ref())
            && line_matches_resolved(line, metadata, with.as_ref(), from.as_ref(), to.as_ref())
    };

    if let Some(c2) = cursor2 {
        if params.cursor.is_some() {
            return Err(wire_err(
                "bad_request",
                "pick one cursor: cursor, or cursor2",
            ));
        }
        let consumed = decode_cursor2(&c2)?;
        if !c2.is_empty() && !cursor_sources_match(&consumed, &names) {
            return Err(wire_err(
                "bad_request",
                "cursor2 journal sources changed; restart the history walk",
            ));
        }
        let (mut lines, next) =
            page_composite(&files, &names, workspace_file, &matches, &consumed, limit);
        crate::mailbox::redact_message_bodies(
            inner.mailbox.as_deref(),
            caller.as_ref().map(|identity| identity.key),
            &mut lines,
        );
        let result = HistoryResult {
            lines,
            next_cursor: None,
            next_cursor2: next.as_ref().map(encode_cursor2),
        };
        return Ok(serde_json::to_value(result).expect("history result serializes"));
    }

    if files.len() > 1 {
        if params.cursor.is_some() {
            return Err(wire_err(
                "bad_request",
                "cursor seqs are per-journal and would skip messages with several sources; \
                 page with cursor2 (opaque, from next_cursor2; empty string starts from the \
                 beginning)",
            ));
        }
        // Tail across the merged record; the returned cursor2 marks
        // everything read so far as consumed, so a resumed walk only sees
        // newer messages (the single-session next_cursor contract).
        let mut msgs = merge_files(&files, workspace_file);
        msgs.retain(&matches);
        let excess = msgs.len().saturating_sub(limit);
        msgs.drain(..excess);
        crate::mailbox::redact_message_bodies(
            inner.mailbox.as_deref(),
            caller.as_ref().map(|identity| identity.key),
            &mut msgs,
        );
        let next_cursor2 = (!msgs.is_empty()).then(|| {
            let mut consumed: BTreeMap<String, u64> = BTreeMap::new();
            for (fi, file) in files.iter().enumerate() {
                let max = file
                    .iter()
                    .filter(|l| matches!(l.kind, Kind::Msg | Kind::Fyi))
                    .map(|l| l.seq)
                    .max();
                consumed.insert(names[fi].clone(), max.unwrap_or(0));
            }
            encode_cursor2(&consumed)
        });
        let result = HistoryResult {
            lines: msgs,
            next_cursor: None,
            next_cursor2,
        };
        return Ok(serde_json::to_value(result).expect("history result serializes"));
    }

    // Single watched session: the shipped path, unchanged.
    let mut msgs = merge_files(&files, workspace_file);
    msgs.retain(&matches);
    let (mut lines, next_cursor) = page(msgs, params.cursor, limit);
    crate::mailbox::redact_message_bodies(
        inner.mailbox.as_deref(),
        caller.as_ref().map(|identity| identity.key),
        &mut lines,
    );
    let result = HistoryResult {
        lines,
        next_cursor,
        next_cursor2: None,
    };
    Ok(serde_json::to_value(result).expect("history result serializes"))
}

/// msg.thread: one id resolves to its folded msg line, every state/gate
/// line sharing the id, and every msg whose reply_to chains to it (each
/// folded), ordered oldest first.
pub(crate) fn msg_thread(inner: &Arc<Inner>, id: &str, peer: Peer) -> Result<Value, WireError> {
    if id.is_empty() {
        return Err(wire_err("bad_request", "msg.thread needs a message id"));
    }
    let caller = history_caller(inner, peer)?;
    let (files, _, workspace_file) = history_sources(inner);
    let metadata = message_metadata(&files);
    let visible_ids: HashSet<String> = merge_files(&files, workspace_file)
        .into_iter()
        .filter(|line| line_visible_to(metadata.get(&line.id), caller.as_ref()))
        .map(|line| line.id)
        .collect();
    if !visible_ids.contains(id) {
        return Err(no_such_message(id));
    }
    match thread_lines(&files, workspace_file, id) {
        Some(mut lines) => {
            lines.retain(|line| visible_ids.contains(&line.id));
            crate::mailbox::redact_message_bodies(
                inner.mailbox.as_deref(),
                caller.as_ref().map(|identity| identity.key),
                &mut lines,
            );
            let result = ThreadResult { lines };
            Ok(serde_json::to_value(result).expect("thread result serializes"))
        }
        None => Err(no_such_message(id)),
    }
}

fn no_such_message(id: &str) -> WireError {
    wire_err(
        "no_such_message",
        format!("no message {id:?} in the record. Run cyclops history to see what's there."),
    )
}

// ---------------------------------------------------------------------------
// Identity ("me")
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ResolvedFilter {
    display: String,
    durable: Option<RecipientKey>,
}

/// Resolve "me" through the same durable identity boundary as sending.
/// Plain labels remain presentation filters for compatibility records.
fn resolve_filter(
    inner: &Arc<Inner>,
    peer: Peer,
    field: Option<String>,
    caller: Option<&MailboxIdentity>,
) -> Result<Option<ResolvedFilter>, WireError> {
    match field {
        Some(value) if value == "me" => {
            if let Some(caller) = caller {
                Ok(Some(ResolvedFilter {
                    display: caller.label.clone(),
                    durable: Some(caller.key),
                }))
            } else {
                Ok(Some(ResolvedFilter {
                    display: caller_name(inner, peer)?,
                    durable: None,
                }))
            }
        }
        Some(display) => Ok(Some(ResolvedFilter {
            display,
            durable: None,
        })),
        None => Ok(None),
    }
}

/// Authenticate workspace readers once and use that identity for both
/// visibility and any `me` filter. Legacy-only daemons retain their
/// existing presentation-label behavior.
fn history_caller(inner: &Arc<Inner>, peer: Peer) -> Result<Option<MailboxIdentity>, WireError> {
    if inner.mailbox.is_none() {
        return Ok(None);
    }
    crate::server::mailbox_caller(inner, peer).map(|(_, caller)| Some(caller))
}

/// The caller as the ledger records it: pane label, pane id, or "admin".
/// Same fail-closed contract as msg.send: no peer credentials or a foreign
/// uid cannot claim an identity.
fn caller_name(inner: &Arc<Inner>, peer: Peer) -> Result<String, WireError> {
    let Some(conn) = peer else {
        return Err(wire_err(
            "denied",
            "can't tell who \"me\" is: peer credentials unavailable",
        ));
    };
    // Asked again, now: the process that opened this connection can have
    // exited, been replaced at the same number, or re-executed since.
    let Some(id) = conn.current() else {
        return Err(wire_err(
            "denied",
            "can't tell who \"me\" is: the process that opened this connection is no longer the one on it",
        ));
    };
    let daemon_uid = unsafe { libc::getuid() };
    if id.uid != daemon_uid {
        return Err(wire_err(
            "denied",
            format!(
                "can't tell who \"me\" is: uid {} is not the daemon's user",
                id.uid
            ),
        ));
    }
    match identity::resolve_sender(id.uid, id.pid, &sender_panes(inner), |p| {
        crate::fusion::is_vendor_now(inner, p)
    }) {
        identity::Sender::Agent(label) => Ok(label),
        identity::Sender::Pane(pane_id) => Ok(pane_id),
        identity::Sender::Admin => Ok("admin".to_string()),
        // Same rule as sending: a chain that could not be walked is not
        // the operator, and answering "me" with the operator's history
        // would hand somebody else's messages to whoever asked.
        identity::Sender::Unprovable => Err(wire_err(
            "denied",
            "can't tell who \"me\" is: the calling process could not be placed".to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Read model
// ---------------------------------------------------------------------------

/// Workspace messages first, followed by pre-upgrade session journals.
/// Empty workspace journals are omitted so an untouched installation keeps
/// the existing single-session cursor contract.
fn history_sources(inner: &Inner) -> (Vec<Vec<LedgerLine>>, Vec<String>, Option<usize>) {
    let mut files = Vec::new();
    let mut names = Vec::new();
    let mut workspace_file = None;
    let mut seen = HashSet::new();
    if let Some(service) = &inner.mailbox {
        match service.journal_lines() {
            Ok(lines) if !lines.is_empty() => {
                workspace_file = Some(files.len());
                names.push(format!("workspace:{}", service.workspace_id()));
                files.push(lines);
            }
            Ok(_) => {}
            Err(error) => {
                warn!(error = %error, "workspace message history is unreadable");
            }
        }
    }
    for slot in inner.session_slots() {
        let source = legacy_journal_source(&slot);
        if seen.insert(source.clone()) {
            names.push(source);
            files.push(read_session(&slot));
        }
    }
    (files, names, workspace_file)
}

fn legacy_journal_source(slot: &crate::SessionSlot) -> String {
    let file = slot
        .ledger
        .path()
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    format!("session-journal:{file}")
}

fn read_session(slot: &crate::SessionSlot) -> Vec<LedgerLine> {
    match slot.ledger.read_after(0) {
        Ok(lines) => lines,
        Err(e) => {
            warn!(session = %slot.name(), error = %e, "history read failed; treating as empty");
            Vec::new()
        }
    }
}

/// Fold one file's delivery-state lines into its msg/fyi lines. Returns the
/// folded messages in file order. Later state lines win: within one file
/// they are appended in transition order.
pub(crate) fn fold_messages(lines: &[LedgerLine]) -> Vec<LedgerLine> {
    let mut msgs: Vec<LedgerLine> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for line in lines {
        match line.kind {
            Kind::Msg | Kind::Fyi => {
                index.insert(line.id.clone(), msgs.len());
                let mut msg = line.clone();
                // The hosted routing list is delivery-pipeline bookkeeping,
                // not message content; the folded read model drops it.
                msg.data = None;
                msgs.push(msg);
            }
            Kind::State => {
                // Delivery-state lines carry the full latest record in
                // deliveries[0]; fused-state lines carry none and are
                // skipped.
                let Some(record) = line.deliveries.first() else {
                    continue;
                };
                let is_delivery = line
                    .data
                    .as_ref()
                    .is_some_and(|d| d.get("to_state").is_some());
                if !is_delivery {
                    continue;
                }
                if let Some(&i) = index.get(&line.id) {
                    let msg = &mut msgs[i];
                    match msg.deliveries.iter_mut().find(|d| d.to == record.to) {
                        Some(d) => *d = record.clone(),
                        None => msg.deliveries.push(record.clone()),
                    }
                }
            }
            _ => {}
        }
    }
    msgs
}

/// Merge per-file folded messages into one stream. Compatibility copies
/// merge only when no workspace record owns their id. Other copies dedupe
/// by id, and each recipient keeps its newest delivery record. Output is
/// ordered oldest first.
pub(crate) fn merge_files(
    files: &[Vec<LedgerLine>],
    workspace_file: Option<usize>,
) -> Vec<LedgerLine> {
    let workspace_ids = workspace_record_ids(files, workspace_file);
    let mut merged: Vec<LedgerLine> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (file_index, file) in files.iter().enumerate() {
        for msg in fold_messages(file) {
            if workspace_ids.contains(msg.id.as_str()) && Some(file_index) != workspace_file {
                continue;
            }
            match index.get(&msg.id) {
                None => {
                    index.insert(msg.id.clone(), merged.len());
                    merged.push(msg);
                }
                Some(&i) => {
                    let kept = &mut merged[i];
                    for d in msg.deliveries {
                        match kept.deliveries.iter_mut().find(|k| k.to == d.to) {
                            Some(k) => {
                                // Newest transition wins; on a ts tie the
                                // advanced record beats the untouched
                                // queued copy.
                                let advanced = d.ts > k.ts
                                    || (d.ts == k.ts
                                        && k.state == cyclops_proto::DeliveryState::Queued
                                        && d.state != cyclops_proto::DeliveryState::Queued);
                                if advanced {
                                    *k = d;
                                }
                            }
                            None => kept.deliveries.push(d),
                        }
                    }
                }
            }
        }
    }
    sort_lines(&mut merged);
    merged
}

/// Every delivery whose latest folded state still needs a human, oldest
/// first. Same read model `msg.history` uses, so the answer does not
/// depend on how much of the record a caller happens to be holding: a
/// quota park from hours ago reads exactly like one from a second ago.
pub(crate) fn open_deliveries(inner: &Inner) -> Vec<OpenDelivery> {
    let (files, _, workspace_file) = history_sources(inner);
    let mut open = open_from(&files, workspace_file);
    if workspace_file.is_none() {
        let Some(service) = &inner.mailbox else {
            return open;
        };
        let workspace_ids = match service.workspace_message_ids() {
            Ok(ids) => ids,
            Err(error) => {
                // The compatibility journals cannot safely answer which
                // copy owns a message when workspace ownership is unreadable.
                warn!(error = %error, "open delivery ownership is unreadable");
                return Vec::new();
            }
        };
        open.retain(|delivery| !workspace_ids.contains(&delivery.id));
    }
    open
}

/// The fold half of [`open_deliveries`], split out so it tests on files.
pub(crate) fn open_from(
    files: &[Vec<LedgerLine>],
    workspace_file: Option<usize>,
) -> Vec<OpenDelivery> {
    let recipients = delivery_recipients(files);
    let mut out = Vec::new();
    for msg in merge_files(files, workspace_file) {
        for d in &msg.deliveries {
            // The rule lives in cyclops_proto::attention and is read, not
            // restated: the daemon's answer and the eye that draws it must
            // never disagree about what an open delivery is.
            if cyclops_proto::delivery_needs_human(d.state) {
                out.push(OpenDelivery {
                    id: msg.id.clone(),
                    to: d.to.clone(),
                    recipient: recipients
                        .get(&(msg.id.clone(), d.to.clone()))
                        .map(|(_, recipient)| *recipient),
                    state: d.state,
                    ts: d.ts,
                    cause: d.cause.clone(),
                });
            }
        }
    }
    out
}

/// New delivery transition rows carry an immutable recipient beside the
/// presentation label. Fold the newest valid key without changing the
/// legacy delivery record shape.
fn delivery_recipients(
    files: &[Vec<LedgerLine>],
) -> HashMap<(String, String), ((u64, u64), RecipientKey)> {
    let mut recipients = HashMap::new();
    for line in files.iter().flatten() {
        if line.kind != Kind::State {
            continue;
        }
        let Some(data) = &line.data else { continue };
        if data.get("to_state").is_none() {
            continue;
        }
        let (Some(to), Ok(recipient)) = (
            data.get("to").and_then(Value::as_str),
            serde_json::from_value::<RecipientKey>(
                data.get("recipient").cloned().unwrap_or(Value::Null),
            ),
        ) else {
            continue;
        };
        let key = (line.id.clone(), to.to_string());
        let rank = (line.ts, line.seq);
        if recipients
            .get(&key)
            .is_none_or(|(current, _)| rank >= *current)
        {
            recipients.insert(key, (rank, recipient));
        }
    }
    recipients
}

/// Order lines oldest first. ts is the cross-file comparator; seq breaks
/// ties within a file (with one watched session, the shipped norm, this is
/// exactly the append order).
fn sort_lines(lines: &mut [LedgerLine]) {
    lines.sort_by(|a, b| (a.ts, a.seq, &a.id).cmp(&(b.ts, b.seq, &b.id)));
}

fn message_metadata(files: &[Vec<LedgerLine>]) -> HashMap<String, MessageMetadata> {
    let mut metadata = HashMap::new();
    for file in files {
        for line in file {
            if !matches!(line.kind, Kind::Msg | Kind::Fyi) {
                continue;
            }
            let Some(data) = &line.data else {
                continue;
            };
            if let Ok(value) = serde_json::from_value::<MessageMetadata>(data.clone()) {
                metadata.entry(line.id.clone()).or_insert(value);
            }
        }
    }
    metadata
}

/// Workspace administrators can inspect the whole workspace. Agents can
/// inspect only messages they sent or received. Durable metadata is
/// authoritative for workspace records. Pre-upgrade records have no stable
/// participant identity, so agents cannot read them. Admin retains the
/// compatibility view.
fn line_visible_to(metadata: Option<&MessageMetadata>, caller: Option<&MailboxIdentity>) -> bool {
    let Some(caller) = caller else {
        return true;
    };
    if caller.key.is_admin() {
        return true;
    }
    match metadata {
        Some(metadata) => {
            metadata.sender == caller.key || metadata.recipients.contains(&caller.key)
        }
        None => false,
    }
}

fn sender_matches(
    line: &LedgerLine,
    metadata: Option<&MessageMetadata>,
    filter: &ResolvedFilter,
) -> bool {
    match (filter.durable, metadata) {
        (Some(expected), Some(metadata)) => metadata.sender == expected,
        _ => line.from == filter.display,
    }
}

fn recipient_matches(
    line: &LedgerLine,
    metadata: Option<&MessageMetadata>,
    filter: &ResolvedFilter,
) -> bool {
    match (filter.durable, metadata) {
        (Some(expected), Some(metadata)) => metadata.recipients.contains(&expected),
        _ => line.to.iter().any(|recipient| recipient == &filter.display),
    }
}

fn line_matches_resolved(
    line: &LedgerLine,
    metadata: Option<&MessageMetadata>,
    with: Option<&ResolvedFilter>,
    from: Option<&ResolvedFilter>,
    to: Option<&ResolvedFilter>,
) -> bool {
    if let Some(filter) = with {
        if !sender_matches(line, metadata, filter) && !recipient_matches(line, metadata, filter) {
            return false;
        }
    }
    if let Some(filter) = from {
        if !sender_matches(line, metadata, filter) {
            return false;
        }
    }
    if let Some(filter) = to {
        if !recipient_matches(line, metadata, filter) {
            return false;
        }
    }
    true
}

/// One message against the resolved filters. `with` means from or to.
#[cfg(test)]
pub(crate) fn line_matches(
    l: &LedgerLine,
    with: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
) -> bool {
    if let Some(w) = with {
        if l.from != w && !l.to.iter().any(|t| t == w) {
            return false;
        }
    }
    if let Some(f) = from {
        if l.from != f {
            return false;
        }
    }
    if let Some(t) = to {
        if !l.to.iter().any(|x| x == t) {
            return false;
        }
    }
    true
}

/// Apply cursor and limit to an oldest-first stream.
///
/// No cursor: the newest `limit` messages (the tail a human asks for).
/// With a cursor: the oldest `limit` messages recorded after it, so a
/// script walking forward covers everything without gaps. Either way the
/// result stays oldest-first and next_cursor names the newest line
/// returned. The cursor compares against the msg line's seq: a message
/// counts as "after" the cursor when its msg fact was recorded after it,
/// even if its delivery chain is still advancing.
pub(crate) fn page(
    mut lines: Vec<LedgerLine>,
    cursor: Option<u64>,
    limit: usize,
) -> (Vec<LedgerLine>, Option<u64>) {
    match cursor {
        Some(c) => {
            lines.retain(|l| l.seq > c);
            lines.truncate(limit);
        }
        None => {
            let excess = lines.len().saturating_sub(limit);
            lines.drain(..excess);
        }
    }
    let next_cursor = lines.last().map(|l| l.seq);
    (lines, next_cursor)
}

// ---------------------------------------------------------------------------
// Composite paging (several watched sessions)
// ---------------------------------------------------------------------------

/// Encode a composite cursor: hex of the JSON {session: seq} map. Hex keeps
/// it a single opaque token on the wire; clients pass it back verbatim.
pub(crate) fn encode_cursor2(consumed: &BTreeMap<String, u64>) -> String {
    let json = serde_json::to_string(consumed).expect("cursor map serializes");
    let mut out = String::with_capacity(json.len() * 2);
    for b in json.as_bytes() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Decode a composite cursor. The empty string is the explicit
/// start-of-record token; anything else must be a cursor this daemon
/// issued.
pub(crate) fn decode_cursor2(s: &str) -> Result<BTreeMap<String, u64>, WireError> {
    if s.is_empty() {
        return Ok(BTreeMap::new());
    }
    let bad = || wire_err("bad_request", "cursor2 is not a cursor this daemon issued");
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return Err(bad());
    }
    let mut bytes = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| bad())?);
    }
    serde_json::from_slice(&bytes).map_err(|_| bad())
}

fn cursor_sources_match(consumed: &BTreeMap<String, u64>, names: &[String]) -> bool {
    consumed.len() == names.len() && names.iter().all(|name| consumed.contains_key(name))
}

/// Forward paging over several session files: a k-way merge over the
/// per-file msg streams with one consumed-prefix position per session.
///
/// Within one file msg lines are appended in (ts, seq) order, so
/// "seq <= consumed[session]" is exactly the prefix an earlier page
/// walked; across files the merge order is the (ts, seq, id, file) key. A
/// message written to several files (cross-session broadcast) is emitted
/// only at its earliest copy in that order and its later copies are
/// consumed silently, so a walk returns every message exactly once, in
/// merge order, with no gaps: per-file order is never violated, and a
/// copy can never slip behind its file's consumed position.
///
/// Non-matching messages are consumed without being emitted (filters
/// narrow the output, never the walk). Emitted lines carry the cross-file
/// folded delivery records, same read model as [`merge_files`].
pub(crate) fn page_composite(
    files: &[Vec<LedgerLine>],
    names: &[String],
    workspace_file: Option<usize>,
    is_match: &dyn Fn(&LedgerLine) -> bool,
    consumed: &BTreeMap<String, u64>,
    limit: usize,
) -> (Vec<LedgerLine>, Option<BTreeMap<String, u64>>) {
    // Content: the merged fold, so every emitted line shows the current
    // per-recipient delivery states wherever they were recorded.
    let merged = merge_files(files, workspace_file);
    let content: HashMap<&str, &LedgerLine> = merged.iter().map(|l| (l.id.as_str(), l)).collect();

    let workspace_ids = workspace_record_ids(files, workspace_file);

    // Every authoritative or compatibility-only msg line in global order.
    let mut copies: Vec<(u64, u64, &str, usize)> = Vec::new();
    for (fi, file) in files.iter().enumerate() {
        for l in file {
            let shadowed = workspace_ids.contains(l.id.as_str()) && Some(fi) != workspace_file;
            if matches!(l.kind, Kind::Msg | Kind::Fyi) && !shadowed {
                copies.push((l.ts, l.seq, l.id.as_str(), fi));
            }
        }
    }
    copies.sort_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));

    let mut next: BTreeMap<String, u64> = consumed.clone();
    for name in names {
        next.entry(name.clone()).or_insert(0);
    }
    let mut out: Vec<LedgerLine> = Vec::new();
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for (_, seq, id, fi) in &copies {
        // First copy in merge order is the emitting one; computed over the
        // FULL list so a copy whose primary was consumed pages ago still
        // reads as the duplicate it is.
        let is_primary = seen_ids.insert(id);
        let name = names.get(*fi).map(String::as_str).unwrap_or_default();
        if consumed.get(name).copied().unwrap_or(0) >= *seq {
            continue; // walked by an earlier page
        }
        if out.len() == limit {
            break; // page full; stop consuming
        }
        next.insert(name.to_string(), *seq);
        if is_primary {
            if let Some(line) = content.get(id) {
                if is_match(line) {
                    out.push((*line).clone());
                }
            }
        }
    }
    if out.is_empty() {
        // Nothing left to say: end of walk, same signal as the
        // single-session pager.
        return (out, None);
    }
    (out, Some(next))
}

fn workspace_record_ids(files: &[Vec<LedgerLine>], workspace_file: Option<usize>) -> HashSet<&str> {
    workspace_file
        .and_then(|index| files.get(index))
        .into_iter()
        .flatten()
        .map(|line| line.id.as_str())
        .collect()
}

/// Assemble one thread: the folded msg line for `id`, every state/gate line
/// sharing `id`, and the folded msg line of every reply chaining to it.
/// None when the record has no line with that id at all.
pub(crate) fn thread_lines(
    files: &[Vec<LedgerLine>],
    workspace_file: Option<usize>,
    id: &str,
) -> Option<Vec<LedgerLine>> {
    // Chain lines: state and gate lines carrying the id. Copies of the same
    // transition land in every session file hosting the delivery, so
    // cross-file duplicates collapse on content (everything but seq).
    let mut chain: Vec<LedgerLine> = Vec::new();
    let mut seen_chain: HashSet<String> = HashSet::new();
    let mut id_exists = false;
    let workspace_ids = workspace_record_ids(files, workspace_file);
    for (file_index, file) in files.iter().enumerate() {
        for line in file {
            if line.id != id {
                continue;
            }
            if workspace_ids.contains(line.id.as_str()) && Some(file_index) != workspace_file {
                continue;
            }
            id_exists = true;
            if matches!(line.kind, Kind::Msg | Kind::Fyi) {
                continue; // represented by the folded copy below
            }
            let key = format!(
                "{:?}|{}|{}|{}|{}",
                line.kind,
                line.ts,
                line.from,
                line.to.join(","),
                line.data.as_ref().map(Value::to_string).unwrap_or_default(),
            );
            if seen_chain.insert(key) {
                chain.push(line.clone());
            }
        }
    }
    if !id_exists {
        return None;
    }

    // Reply walk over the folded message set: id -> direct replies ->
    // replies to those, until the frontier empties.
    let msgs = merge_files(files, workspace_file);
    let mut wanted: HashSet<&str> = HashSet::new();
    wanted.insert(id);
    loop {
        let mut grew = false;
        for m in &msgs {
            if wanted.contains(m.id.as_str()) {
                continue;
            }
            if m.reply_to.as_deref().is_some_and(|r| wanted.contains(r)) {
                wanted.insert(m.id.as_str());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    let mut lines: Vec<LedgerLine> = msgs
        .iter()
        .filter(|m| wanted.contains(m.id.as_str()))
        .cloned()
        .collect();
    lines.extend(chain);
    sort_lines(&mut lines);
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::{MailboxDirectory, MailboxSend, MailboxService, MessageStore};
    use cyclops_proto::{
        Delivery, DeliveryState, MessageId, MessagePresentation, RecipientPresentation,
        RequestDigest, SessionInstanceId, TmuxPaneId, VerifiedBy, WorkspaceId,
        CANONICAL_RECORD_VERSION,
    };
    use std::path::Path;
    use std::str::FromStr;

    fn fixture() -> Vec<LedgerLine> {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/history.ndjson");
        let home = cyclops_proto::scratch::scratch_dir(&format!(
            "cyc-history-fixture-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let root = cyclops_state::StateRoot::open_or_create(&home).expect("state root opens");
        let descendant = Path::new("ledger/history.ndjson");
        let mut file = root.open_append(descendant).expect("fixture opens");
        std::io::Write::write_all(&mut file, &std::fs::read(source).expect("fixture reads"))
            .expect("fixture copies");
        let lines = cyclops_ledger::read_after(&root, descendant, 0).expect("fixture replays");
        drop(file);
        drop(root);
        let _ = std::fs::remove_dir_all(home);
        lines
    }

    fn folded() -> Vec<LedgerLine> {
        merge_files(&[fixture()], None)
    }

    fn ids(lines: &[LedgerLine]) -> Vec<&str> {
        lines.iter().map(|l| l.id.as_str()).collect()
    }

    fn workspace_identity(session: &str, pane: &str, label: &str) -> MailboxIdentity {
        let workspace = WorkspaceId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        MailboxIdentity {
            key: RecipientKey::agent(
                workspace,
                SessionInstanceId::from_str(session).unwrap(),
                TmuxPaneId::from_str(pane).unwrap(),
            ),
            label: label.into(),
        }
    }

    #[test]
    fn fixture_covers_every_kind_and_skips_the_invalid_line() {
        let lines = fixture();
        // The file carries one deliberately torn line; read_after drops it.
        for kind in [Kind::Msg, Kind::Fyi, Kind::System, Kind::State, Kind::Gate] {
            assert!(
                lines.iter().any(|l| l.kind == kind),
                "fixture is missing {kind:?}"
            );
        }
    }

    #[test]
    fn fold_carries_the_latest_delivery_state_per_recipient() {
        let msgs = folded();
        assert_eq!(
            ids(&msgs),
            vec!["m-aaaaaa", "m-bbbbbb", "m-cccccc", "m-dddddd", "m-eeeeee"]
        );

        let a = &msgs[0];
        assert_eq!(a.deliveries.len(), 1);
        assert_eq!(a.deliveries[0].state, DeliveryState::DeliveredVerified);
        assert_eq!(a.deliveries[0].verified_by, Some(VerifiedBy::Hook));

        // Broadcast: one msg fact, per-recipient current states.
        let b = &msgs[1];
        assert_eq!(b.kind, Kind::Fyi);
        let by_to: HashMap<&str, &Delivery> =
            b.deliveries.iter().map(|d| (d.to.as_str(), d)).collect();
        assert_eq!(
            by_to["reviewer"].state,
            DeliveryState::DeliveredUnverified,
            "{b:?}"
        );
        assert_eq!(
            by_to["implementer"].state,
            DeliveryState::ParkedBlockedQuota
        );

        // In-flight chains fold too; history may honestly show mid-pipeline.
        assert_eq!(msgs[4].deliveries[0].state, DeliveryState::Gating);
    }

    /// The attention seed the stream UI starts from. It must find both
    /// unresolved deliveries in the fixture wherever they sit in the file,
    /// and nothing that resolved itself.
    #[test]
    fn open_deliveries_are_the_two_states_a_human_must_clear() {
        let open = open_from(&[fixture()], None);
        let got: Vec<(&str, &str, DeliveryState)> = open
            .iter()
            .map(|d| (d.id.as_str(), d.to.as_str(), d.state))
            .collect();
        assert_eq!(
            got,
            vec![
                ("m-bbbbbb", "implementer", DeliveryState::ParkedBlockedQuota),
                ("m-dddddd", "admin", DeliveryState::AttentionRequired),
            ]
        );
        // The transition timestamp travels with it: the item is as old as
        // the record says, not as old as the query.
        assert_eq!(open[0].ts, 1_754_000_002_600);
        // Delivered and in-flight chains are nobody's to clear.
        assert!(open
            .iter()
            .all(|d| d.id != "m-aaaaaa" && d.id != "m-eeeeee"));
    }

    #[test]
    fn workspace_ownership_hides_a_legacy_attention_copy() {
        let workspace: Vec<_> = fixture()
            .into_iter()
            .filter(|line| line.id == "m-aaaaaa")
            .collect();
        let mut legacy = workspace.clone();
        for line in &mut legacy {
            let is_delivery_state = line.kind == Kind::State
                && line
                    .data
                    .as_ref()
                    .is_some_and(|data| data.get("to_state").is_some());
            if !is_delivery_state {
                continue;
            }
            line.ts += 10_000;
            line.data.as_mut().unwrap()["to_state"] =
                serde_json::json!(DeliveryState::AttentionRequired);
            line.deliveries[0].state = DeliveryState::AttentionRequired;
            line.deliveries[0].ts = line.ts;
            line.deliveries[0].cause = Some("legacy_ghost".into());
        }

        let files = vec![workspace, legacy];
        assert_eq!(
            open_from(&files, None)[0].state,
            DeliveryState::AttentionRequired,
            "the compatibility copy is open without a workspace owner"
        );
        assert!(
            open_from(&files, Some(0)).is_empty(),
            "workspace ownership must hide the legacy terminal copy"
        );
    }

    #[test]
    fn workspace_ownership_applies_to_every_recipient_of_a_broadcast() {
        let workspace: Vec<_> = fixture()
            .into_iter()
            .filter(|line| line.id == "m-bbbbbb")
            .collect();
        let mut legacy = workspace.clone();
        for line in &mut legacy {
            let targets_reviewer = line.kind == Kind::State
                && line
                    .data
                    .as_ref()
                    .and_then(|data| data.get("to"))
                    .and_then(serde_json::Value::as_str)
                    == Some("reviewer");
            if !targets_reviewer {
                continue;
            }
            line.ts += 10_000;
            line.data.as_mut().unwrap()["to_state"] =
                serde_json::json!(DeliveryState::AttentionRequired);
            line.deliveries[0].state = DeliveryState::AttentionRequired;
            line.deliveries[0].ts = line.ts;
            line.deliveries[0].cause = Some("legacy_ghost".into());
        }

        let files = vec![workspace, legacy];
        assert!(open_from(&files, None)
            .iter()
            .any(|delivery| delivery.to == "reviewer"));
        let canonical = open_from(&files, Some(0));
        assert_eq!(canonical.len(), 1);
        assert_eq!(canonical[0].to, "implementer");
        assert_eq!(canonical[0].state, DeliveryState::ParkedBlockedQuota);
    }

    #[test]
    fn with_filter_means_from_or_to() {
        let mut msgs = folded();
        msgs.retain(|l| line_matches(l, Some("reviewer"), None, None));
        assert_eq!(
            ids(&msgs),
            vec!["m-aaaaaa", "m-bbbbbb", "m-cccccc", "m-eeeeee"]
        );
    }

    #[test]
    fn from_and_to_filter_one_direction_each() {
        let mut from_codex = folded();
        from_codex.retain(|l| line_matches(l, None, Some("codex"), None));
        assert_eq!(ids(&from_codex), vec!["m-aaaaaa", "m-dddddd", "m-eeeeee"]);

        let mut to_codex = folded();
        to_codex.retain(|l| line_matches(l, None, None, Some("codex")));
        assert_eq!(ids(&to_codex), vec!["m-cccccc"]);

        // Combined from/to narrows to the intersection.
        let mut both = folded();
        both.retain(|l| line_matches(l, None, Some("codex"), Some("admin")));
        assert_eq!(ids(&both), vec!["m-dddddd"]);
    }

    #[test]
    fn to_me_is_a_plain_name_match_after_resolution() {
        // The handler resolves "me" to the caller before filtering; the
        // filter itself sees an ordinary name.
        let mut msgs = folded();
        msgs.retain(|l| line_matches(l, None, None, Some("admin")));
        assert_eq!(ids(&msgs), vec!["m-dddddd"]);
    }

    #[test]
    fn durable_participants_bound_agent_history_visibility() {
        let alice = workspace_identity("00000000-0000-0000-0000-000000000002", "%1", "alice");
        let bob = workspace_identity("00000000-0000-0000-0000-000000000003", "%2", "bob");
        // Carol deliberately reuses Alice's display label. Durable metadata
        // must win, or a mutable label can disclose another agent's history.
        let carol = workspace_identity("00000000-0000-0000-0000-000000000004", "%3", "alice");
        let workspace = alice.key.workspace_id();
        let message_id = MessageId::new("m-visible").unwrap();
        let metadata = MessageMetadata {
            record_version: CANONICAL_RECORD_VERSION,
            workspace_id: workspace,
            sender: alice.key,
            recipients: vec![bob.key],
            presentation: MessagePresentation {
                sender_label: alice.label.clone(),
                recipient_labels: vec![RecipientPresentation {
                    recipient: bob.key,
                    label: bob.label.clone(),
                }],
            },
            thread_root: message_id,
            client_key: None,
            request_digest: RequestDigest::compute(
                Kind::Msg,
                alice.key,
                &[bob.key],
                Some("subject"),
                Some("body"),
                None,
                None,
            )
            .unwrap(),
            supersedes: None,
        };
        assert!(line_visible_to(Some(&metadata), Some(&alice)));
        assert!(line_visible_to(Some(&metadata), Some(&bob)));
        assert!(!line_visible_to(Some(&metadata), Some(&carol)));
        assert!(line_visible_to(
            Some(&metadata),
            Some(&MailboxIdentity {
                key: RecipientKey::admin(workspace),
                label: "admin".into(),
            })
        ));

        // Pre-upgrade labels cannot bind a live agent to an old participant.
        // Reusing Alice's label must not disclose the compatibility record.
        assert!(!line_visible_to(None, Some(&carol)));
        let outsider = workspace_identity("00000000-0000-0000-0000-000000000005", "%4", "outsider");
        assert!(!line_visible_to(None, Some(&outsider)));
        assert!(line_visible_to(
            None,
            Some(&MailboxIdentity {
                key: RecipientKey::admin(workspace),
                label: "admin".into(),
            })
        ));
    }

    #[test]
    fn no_cursor_takes_the_newest_tail() {
        let (lines, next) = page(folded(), None, 2);
        assert_eq!(ids(&lines), vec!["m-dddddd", "m-eeeeee"]);
        assert_eq!(next, Some(lines[1].seq));
    }

    #[test]
    fn cursor_walks_forward_without_gaps_or_dupes() {
        let all = folded();
        let mut walked: Vec<String> = Vec::new();
        let mut cursor = Some(0);
        loop {
            let (batch, next) = page(all.clone(), cursor, 2);
            if batch.is_empty() {
                break;
            }
            walked.extend(batch.iter().map(|l| l.id.clone()));
            cursor = next;
        }
        assert_eq!(
            walked,
            vec!["m-aaaaaa", "m-bbbbbb", "m-cccccc", "m-dddddd", "m-eeeeee"]
        );
        // Resuming past the end returns nothing and no cursor.
        let (empty, next) = page(all, cursor, 2);
        assert!(empty.is_empty());
        assert_eq!(next, None);
    }

    // -----------------------------------------------------------------
    // Composite paging (several watched sessions)
    // -----------------------------------------------------------------

    fn msg_line(seq: u64, ts: u64, id: &str, from: &str, to: &str) -> LedgerLine {
        LedgerLine {
            seq,
            boot_id: "b".into(),
            id: id.into(),
            ts,
            kind: Kind::Msg,
            from: from.into(),
            to: vec![to.into()],
            subject: Some(id.into()),
            body: None,
            reply_to: None,
            deliveries: vec![Delivery {
                to: to.into(),
                state: DeliveryState::Queued,
                verified_by: None,
                attempts: 0,
                ts,
                cause: None,
            }],
            data: None,
        }
    }

    fn all(_: &LedgerLine) -> bool {
        true
    }

    fn walk_composite(
        files: &[Vec<LedgerLine>],
        names: &[String],
        workspace_file: Option<usize>,
        limit: usize,
    ) -> Vec<String> {
        let mut cursor = BTreeMap::new();
        let mut walked = Vec::new();
        loop {
            let (batch, next) = page_composite(files, names, workspace_file, &all, &cursor, limit);
            if batch.is_empty() {
                assert!(next.is_none(), "empty page must end the walk");
                break;
            }
            walked.extend(batch.iter().map(|l| l.id.clone()));
            cursor = next.expect("non-empty page carries a cursor");
        }
        walked
    }

    #[test]
    fn composite_walk_interleaves_two_files_without_gaps_or_dupes() {
        // Two sessions with interleaved timestamps and CLASHING seqs: the
        // raw-seq pager skipped here (seq 2 of file B hides behind seq 2 of
        // file A), which is the M2 blocker this pager replaces.
        let a = vec![
            msg_line(2, 1000, "m-a1", "x", "y"),
            msg_line(4, 1200, "m-a2", "x", "y"),
        ];
        let b = vec![
            msg_line(2, 1100, "m-b1", "x", "y"),
            msg_line(3, 1300, "m-b2", "x", "y"),
        ];
        let names = vec!["alpha".to_string(), "beta".to_string()];
        for limit in 1..=4 {
            assert_eq!(
                walk_composite(&[a.clone(), b.clone()], &names, None, limit),
                vec!["m-a1", "m-b1", "m-a2", "m-b2"],
                "limit {limit}"
            );
        }
    }

    #[test]
    fn workspace_copy_sets_the_cursor_order_for_a_legacy_id_collision() {
        let first = msg_line(1, 200, "m-first", "sender", "recipient");
        let canonical = msg_line(2, 300, "m-collision", "sender", "recipient");
        let mut legacy = canonical.clone();
        legacy.seq = 1;
        legacy.ts = 100;
        legacy.subject = Some("legacy collision".into());
        let files = vec![vec![first, canonical], vec![legacy]];
        let names = vec![
            "workspace:test".to_string(),
            "session-journal:legacy.ndjson".to_string(),
        ];

        assert_eq!(
            walk_composite(&files, &names, Some(0), 1),
            vec!["m-first", "m-collision"]
        );
    }

    #[test]
    fn workspace_message_ignores_a_legacy_delivery_collision() {
        let canonical = msg_line(1, 200, "m-collision", "sender", "recipient");
        let mut legacy = canonical.clone();
        legacy.seq = 7;
        legacy.ts = 300;
        legacy.to = vec!["legacy".into()];
        legacy.deliveries = vec![Delivery {
            to: "legacy".into(),
            state: DeliveryState::AttentionRequired,
            verified_by: None,
            attempts: 1,
            ts: 300,
            cause: Some("legacy_collision".into()),
        }];
        let files = vec![vec![canonical], vec![legacy]];
        let names = vec![
            "workspace:test".to_string(),
            "session-journal:legacy.ndjson".to_string(),
        ];

        let (page, _) = page_composite(&files, &names, Some(0), &all, &BTreeMap::new(), 10);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].deliveries.len(), 1);
        assert_eq!(page[0].deliveries[0].to, "recipient");
    }

    #[test]
    fn workspace_thread_ignores_legacy_state_and_gate_collisions() {
        let message = msg_line(1, 200, "m-collision", "sender", "recipient");
        let chain = |seq, ts, kind, source: &str| LedgerLine {
            seq,
            boot_id: "b".into(),
            id: "m-collision".into(),
            ts,
            kind,
            from: "cyclopsd".into(),
            to: vec!["recipient".into()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: Vec::new(),
            data: Some(serde_json::json!({"source": source})),
        };
        let workspace_state = chain(2, 210, Kind::State, "workspace");
        let mut legacy_message = message.clone();
        legacy_message.seq = 7;
        legacy_message.ts = 100;
        let legacy_state = chain(8, 220, Kind::State, "legacy_state");
        let legacy_gate = chain(9, 230, Kind::Gate, "legacy_gate");
        let files = vec![
            vec![message, workspace_state],
            vec![legacy_message, legacy_state, legacy_gate],
        ];

        let thread = thread_lines(&files, Some(0), "m-collision").expect("thread exists");
        let sources: Vec<_> = thread
            .iter()
            .filter_map(|line| line.data.as_ref()?.get("source")?.as_str())
            .collect();
        assert_eq!(sources, ["workspace"]);
    }

    #[test]
    fn composite_walk_emits_a_cross_file_broadcast_once() {
        // One broadcast written to both files (dup id), page boundary
        // forced right at it by limit 1: no dupe, no gap.
        let a = vec![
            msg_line(1, 1000, "m-solo1", "x", "left"),
            msg_line(2, 1050, "m-cast0", "x", "left"),
        ];
        let b = vec![
            msg_line(1, 1050, "m-cast0", "x", "right"),
            msg_line(2, 1200, "m-solo2", "x", "right"),
        ];
        let names = vec!["alpha".to_string(), "beta".to_string()];
        for limit in 1..=3 {
            assert_eq!(
                walk_composite(&[a.clone(), b.clone()], &names, None, limit),
                vec!["m-solo1", "m-cast0", "m-solo2"],
                "limit {limit}"
            );
        }
    }

    #[test]
    fn composite_walk_survives_cross_file_clock_skew() {
        // The (ts, seq, id) sort-key cursor's failure shape: file A's copy
        // of the broadcast carries an EARLIER ts than file B's neighbors,
        // so a flat key cursor placed at the broadcast would skip m-b1.
        // Per-file consumed positions cannot.
        let a = vec![msg_line(7, 1004, "m-cast0", "x", "left")];
        let b = vec![
            msg_line(4, 1005, "m-b1", "x", "right"),
            msg_line(5, 1005, "m-cast0", "x", "right"),
            msg_line(6, 1006, "m-b2", "x", "right"),
        ];
        let names = vec!["alpha".to_string(), "beta".to_string()];
        for limit in 1..=3 {
            assert_eq!(
                walk_composite(&[a.clone(), b.clone()], &names, None, limit),
                vec!["m-cast0", "m-b1", "m-b2"],
                "limit {limit}"
            );
        }
    }

    #[test]
    fn composite_filters_narrow_output_but_not_the_walk() {
        let a = vec![
            msg_line(1, 1000, "m-yes1", "codex", "y"),
            msg_line(2, 1100, "m-no10", "other", "y"),
        ];
        let b = vec![msg_line(1, 1200, "m-yes2", "codex", "y")];
        let names = vec!["alpha".to_string(), "beta".to_string()];
        let from_codex = |l: &LedgerLine| l.from == "codex";
        let (batch, next) = page_composite(
            &[a.clone(), b.clone()],
            &names,
            None,
            &from_codex,
            &BTreeMap::new(),
            1,
        );
        assert_eq!(batch[0].id, "m-yes1");
        let (batch, next) = page_composite(&[a, b], &names, None, &from_codex, &next.unwrap(), 1);
        assert_eq!(
            batch[0].id, "m-yes2",
            "the non-matching line was walked over"
        );
        let _ = next;
    }

    #[test]
    fn cursor2_roundtrips_and_rejects_garbage() {
        let mut map = BTreeMap::new();
        map.insert("main".to_string(), 42u64);
        map.insert("side".to_string(), 7u64);
        assert_eq!(decode_cursor2(&encode_cursor2(&map)).unwrap(), map);
        // The empty string is the explicit start-of-record token.
        assert!(decode_cursor2("").unwrap().is_empty());
        for bad in ["zz", "abc", "7b7", "deadbeef", "总"] {
            assert!(decode_cursor2(bad).is_err(), "{bad:?} must not decode");
        }
    }

    #[test]
    fn thread_gathers_chain_lines_and_reply_descendants() {
        let files = [fixture()];
        let lines = thread_lines(&files, None, "m-aaaaaa").expect("thread exists");

        // The root msg is folded (delivered), never the raw queued copy.
        let root = lines
            .iter()
            .find(|l| l.kind == Kind::Msg && l.id == "m-aaaaaa")
            .expect("root msg");
        assert_eq!(root.deliveries[0].state, DeliveryState::DeliveredVerified);
        assert_eq!(
            lines
                .iter()
                .filter(|l| matches!(l.kind, Kind::Msg | Kind::Fyi) && l.id == "m-aaaaaa")
                .count(),
            1,
            "one msg fact"
        );

        // Delivery chain and gate lines share the id and come along.
        assert!(lines.iter().any(|l| l.kind == Kind::Gate));
        let states: Vec<&str> = lines
            .iter()
            .filter(|l| l.kind == Kind::State)
            .filter_map(|l| l.data.as_ref()?.get("to_state")?.as_str())
            .collect();
        assert_eq!(
            states,
            vec![
                "gating",
                "pasting",
                "staged",
                "submitted",
                "delivered_verified"
            ]
        );

        // Replies chain transitively: the reply and the reply to the reply.
        assert!(lines.iter().any(|l| l.id == "m-cccccc"));
        assert!(lines.iter().any(|l| l.id == "m-eeeeee"));
        // Unrelated messages stay out.
        assert!(!lines.iter().any(|l| l.id == "m-bbbbbb"));
        assert!(!lines.iter().any(|l| l.id == "m-dddddd"));

        // Ordered oldest first.
        let mut ts: Vec<u64> = lines.iter().map(|l| l.ts).collect();
        let sorted = ts.clone();
        ts.sort_unstable();
        assert_eq!(ts, sorted);
    }

    #[test]
    fn thread_of_a_reply_holds_its_descendants_not_its_root() {
        let files = [fixture()];
        let lines = thread_lines(&files, None, "m-cccccc").expect("thread exists");
        assert!(lines.iter().any(|l| l.id == "m-eeeeee"), "descendant");
        assert!(
            !lines.iter().any(|l| l.id == "m-aaaaaa"),
            "the brief scopes threads to descendants"
        );
    }

    #[test]
    fn thread_unknown_id_is_none() {
        assert!(thread_lines(&[fixture()], None, "m-nope00").is_none());
    }

    #[test]
    fn cross_session_broadcast_merges_to_one_fact_with_the_hosting_chains() {
        // One broadcast written to two session files. Each file hosts one
        // recipient's chain; the other recipient stays queued in that copy.
        let msg = |seq: u64, hosted: &str| LedgerLine {
            seq,
            boot_id: "b".into(),
            id: "m-xcast0".into(),
            ts: 1000,
            kind: Kind::Msg,
            from: "admin".into(),
            to: vec!["left".into(), "right".into()],
            subject: Some("cross".into()),
            body: None,
            reply_to: None,
            deliveries: vec![
                Delivery {
                    to: "left".into(),
                    state: DeliveryState::Queued,
                    verified_by: None,
                    attempts: 0,
                    ts: 1000,
                    cause: None,
                },
                Delivery {
                    to: "right".into(),
                    state: DeliveryState::Queued,
                    verified_by: None,
                    attempts: 0,
                    ts: 1000,
                    cause: None,
                },
            ],
            data: Some(serde_json::json!({ "hosted": [hosted] })),
        };
        let state = |seq: u64, to: &str, state: DeliveryState, ts: u64| LedgerLine {
            seq,
            boot_id: "b".into(),
            id: "m-xcast0".into(),
            ts,
            kind: Kind::State,
            from: "cyclopsd".into(),
            to: vec![to.into()],
            subject: None,
            body: None,
            reply_to: None,
            deliveries: vec![Delivery {
                to: to.into(),
                state,
                verified_by: Some(VerifiedBy::Screen),
                attempts: 1,
                ts,
                cause: None,
            }],
            data: Some(serde_json::json!({"to": to, "to_state": state})),
        };
        let file_a = vec![
            msg(1, "left"),
            state(2, "left", DeliveryState::DeliveredUnverified, 1200),
        ];
        let file_b = vec![
            msg(1, "right"),
            state(2, "right", DeliveryState::AttentionRequired, 1300),
        ];
        let merged = merge_files(&[file_a, file_b], None);
        assert_eq!(merged.len(), 1, "one msg fact");
        let by_to: HashMap<&str, DeliveryState> = merged[0]
            .deliveries
            .iter()
            .map(|d| (d.to.as_str(), d.state))
            .collect();
        assert_eq!(by_to["left"], DeliveryState::DeliveredUnverified);
        assert_eq!(by_to["right"], DeliveryState::AttentionRequired);
        // Bookkeeping data does not leak into the read model.
        assert!(merged[0].data.is_none());
    }

    #[test]
    fn workspace_message_wins_a_legacy_id_and_body_collision() {
        let sender = workspace_identity("00000000-0000-0000-0000-000000000002", "%1", "sender");
        let recipient =
            workspace_identity("00000000-0000-0000-0000-000000000002", "%2", "recipient");
        let workspace = sender.key.workspace_id();
        let home = cyclops_proto::scratch::scratch_dir(&format!(
            "cyc-history-collision-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let root = cyclops_state::StateRoot::open_or_create(&home).unwrap();
        let directory =
            MailboxDirectory::new(workspace, [sender.clone(), recipient.clone()]).unwrap();
        let store = MessageStore::open(
            &root,
            Path::new("workspaces/current/messages.ndjson"),
            workspace,
            "boot",
        )
        .unwrap();
        let service = MailboxService::new(directory, store);
        let accepted = service
            .send(
                sender.clone(),
                MailboxSend {
                    addresses: vec![recipient.label.clone()],
                    recipient_keys: None,
                    subject: "canonical subject".into(),
                    body: "canonical body".into(),
                    fyi: false,
                    client_key: None,
                    supersedes: None,
                },
            )
            .unwrap();
        let workspace_lines = service.journal_lines().unwrap();
        let mut legacy = workspace_lines
            .iter()
            .find(|line| line.id == accepted.message_id.as_str())
            .expect("workspace message")
            .clone();
        legacy.boot_id = "legacy".into();
        legacy.subject = Some("legacy subject".into());
        legacy.body = Some("legacy collision body".into());
        legacy.data = None;
        let files = vec![workspace_lines, vec![legacy]];

        let mut sender_history = merge_files(&files, Some(0));
        crate::mailbox::redact_message_bodies(
            Some(&service),
            Some(sender.key),
            &mut sender_history,
        );
        assert_eq!(sender_history.len(), 1);
        assert_eq!(
            sender_history[0].subject.as_deref(),
            Some("canonical subject")
        );
        assert_eq!(sender_history[0].body.as_deref(), Some("canonical body"));

        let mut recipient_history = merge_files(&files, Some(0));
        crate::mailbox::redact_message_bodies(
            Some(&service),
            Some(recipient.key),
            &mut recipient_history,
        );
        assert_eq!(recipient_history[0].body, None);

        let mut recipient_thread =
            thread_lines(&files, Some(0), accepted.message_id.as_str()).expect("thread exists");
        crate::mailbox::redact_message_bodies(
            Some(&service),
            Some(recipient.key),
            &mut recipient_thread,
        );
        assert_eq!(recipient_thread.len(), 1);
        assert_eq!(recipient_thread[0].body, None);

        service
            .claim(recipient.key, accepted.message_id.clone())
            .unwrap();
        let mut claimed_history = merge_files(&files, Some(0));
        crate::mailbox::redact_message_bodies(
            Some(&service),
            Some(recipient.key),
            &mut claimed_history,
        );
        assert_eq!(claimed_history[0].body.as_deref(), Some("canonical body"));
        let mut claimed_thread =
            thread_lines(&files, Some(0), accepted.message_id.as_str()).expect("thread exists");
        crate::mailbox::redact_message_bodies(
            Some(&service),
            Some(recipient.key),
            &mut claimed_thread,
        );
        assert_eq!(claimed_thread[0].body.as_deref(), Some("canonical body"));
        assert!(claimed_history
            .iter()
            .chain(&claimed_thread)
            .all(|line| line.body.as_deref() != Some("legacy collision body")));

        drop(service);
        drop(root);
        std::fs::remove_dir_all(home).unwrap();
    }
}
