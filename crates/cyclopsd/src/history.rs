//! msg.history and msg.thread: read-side queries over the session ledgers.
//!
//! Reading is free (ADR messaging rule): any same-uid caller may query the
//! whole record. Identity is only resolved when a filter names "me", and
//! that resolution is the same fail-closed peer-credential walk msg.send
//! uses. These methods never write; the ledger files are the M1 writer's.
//!
//! The read model folds each message's delivery chain (the kind=state lines
//! appended after the msg line) back into the msg line's `deliveries`, so a
//! returned line carries the latest recorded state per recipient: one msg
//! fact, N current badges. Folding happens at read time; nothing on disk is
//! rewritten.
//!
//! Reader choice: cyclops-ledger's `read_after` full scan. A session ledger
//! at GOALS scale (10k lines) parses in single-digit milliseconds on this
//! machine, so no indexed reader was added to that crate; the additive
//! index stays a measured-need option, not a speculative one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use cyclops_proto::{
    HistoryParams, HistoryResult, Kind, LedgerLine, OpenDelivery, ThreadResult, WireError,
};
use serde_json::Value;
use tracing::warn;

use crate::identity;
use crate::server::sender_panes;
use crate::Inner;

/// Peer credentials as the connection captured them (uid, pid). None means
/// the kernel could not report them; "me" resolution fails closed on it.
pub(crate) type Peer = Option<(u32, i32)>;

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
    let with = resolve_me(inner, peer, params.with)?;
    let from = resolve_me(inner, peer, params.from)?;
    let to = resolve_me(inner, peer, params.to)?;
    let matches = |l: &LedgerLine| line_matches(l, with.as_deref(), from.as_deref(), to.as_deref());
    let limit = params.limit as usize;

    let files = read_all_sessions(inner);
    let names: Vec<String> = inner.sessions.iter().map(|s| s.name.clone()).collect();

    if let Some(c2) = cursor2 {
        if params.cursor.is_some() {
            return Err(wire_err(
                "bad_request",
                "pick one cursor: cursor, or cursor2",
            ));
        }
        let consumed = decode_cursor2(&c2)?;
        let (lines, next) = page_composite(&files, &names, &matches, &consumed, limit);
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
                "cursor seqs are per-session and would skip messages with several watched \
                 sessions; page with cursor2 (opaque, from next_cursor2; empty string starts \
                 from the beginning), or read the per-session ledger files directly",
            ));
        }
        // Tail across the merged record; the returned cursor2 marks
        // everything read so far as consumed, so a resumed walk only sees
        // newer messages (the single-session next_cursor contract).
        let mut msgs = merge_files(&files);
        msgs.retain(&matches);
        let excess = msgs.len().saturating_sub(limit);
        msgs.drain(..excess);
        let next_cursor2 = (!msgs.is_empty()).then(|| {
            let mut consumed: BTreeMap<String, u64> = BTreeMap::new();
            for (fi, file) in files.iter().enumerate() {
                let max = file
                    .iter()
                    .filter(|l| matches!(l.kind, Kind::Msg | Kind::Fyi))
                    .map(|l| l.seq)
                    .max();
                if let Some(max) = max {
                    consumed.insert(names[fi].clone(), max);
                }
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
    let mut msgs = merge_files(&files);
    msgs.retain(&matches);
    let (lines, next_cursor) = page(msgs, params.cursor, limit);
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
pub(crate) fn msg_thread(inner: &Arc<Inner>, id: &str) -> Result<Value, WireError> {
    if id.is_empty() {
        return Err(wire_err("bad_request", "msg.thread needs a message id"));
    }
    let files = read_all_sessions(inner);
    match thread_lines(&files, id) {
        Some(lines) => {
            let result = ThreadResult { lines };
            Ok(serde_json::to_value(result).expect("thread result serializes"))
        }
        None => Err(wire_err(
            "no_such_message",
            format!("no message {id:?} in the record. Run cyclops history to see what's there."),
        )),
    }
}

// ---------------------------------------------------------------------------
// Identity ("me")
// ---------------------------------------------------------------------------

/// Replace the special name "me" with the caller's resolved identity.
/// Any other value, including None, passes through untouched.
fn resolve_me(
    inner: &Arc<Inner>,
    peer: Peer,
    field: Option<String>,
) -> Result<Option<String>, WireError> {
    match field {
        Some(v) if v == "me" => Ok(Some(caller_name(inner, peer)?)),
        other => Ok(other),
    }
}

/// The caller as the ledger records it: pane label, pane id, or "admin".
/// Same fail-closed contract as msg.send: no peer credentials or a foreign
/// uid cannot claim an identity.
fn caller_name(inner: &Arc<Inner>, peer: Peer) -> Result<String, WireError> {
    let Some((uid, pid)) = peer else {
        return Err(wire_err(
            "denied",
            "can't tell who \"me\" is: peer credentials unavailable",
        ));
    };
    let daemon_uid = unsafe { libc::getuid() };
    if uid != daemon_uid {
        return Err(wire_err(
            "denied",
            format!("can't tell who \"me\" is: uid {uid} is not the daemon's user"),
        ));
    }
    Ok(
        match identity::resolve_sender(uid, pid, &sender_panes(inner)) {
            identity::Sender::Agent(label) => label,
            identity::Sender::Pane(pane_id) => pane_id,
            identity::Sender::Admin => "admin".to_string(),
        },
    )
}

// ---------------------------------------------------------------------------
// Read model
// ---------------------------------------------------------------------------

/// Every session ledger replayed, in session order. A file that fails to
/// read degrades to empty with a warning: one corrupt file must not blind
/// the query to the others.
fn read_all_sessions(inner: &Inner) -> Vec<Vec<LedgerLine>> {
    inner
        .sessions
        .iter()
        .map(|slot| match cyclops_ledger::read_after(slot.ledger.path(), 0) {
            Ok(lines) => lines,
            Err(e) => {
                warn!(session = %slot.name, error = %e, "history read failed; treating as empty");
                Vec::new()
            }
        })
        .collect()
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

/// Merge per-file folded messages into one stream. A message written to
/// several session files (cross-session broadcast) is ONE fact: the copies
/// dedupe by id, and per recipient the newest delivery record wins, which
/// is the hosting file's chain. Output is ordered oldest first.
pub(crate) fn merge_files(files: &[Vec<LedgerLine>]) -> Vec<LedgerLine> {
    let mut merged: Vec<LedgerLine> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for file in files {
        for msg in fold_messages(file) {
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
    open_from(&read_all_sessions(inner))
}

/// The fold half of [`open_deliveries`], split out so it tests on files.
pub(crate) fn open_from(files: &[Vec<LedgerLine>]) -> Vec<OpenDelivery> {
    let mut out = Vec::new();
    for msg in merge_files(files) {
        for d in &msg.deliveries {
            // The rule lives in cyclops_proto::attention and is read, not
            // restated: the daemon's answer and the eye that draws it must
            // never disagree about what an open delivery is.
            if cyclops_proto::delivery_needs_human(d.state) {
                out.push(OpenDelivery {
                    id: msg.id.clone(),
                    to: d.to.clone(),
                    state: d.state,
                    ts: d.ts,
                    cause: d.cause.clone(),
                });
            }
        }
    }
    out
}

/// Order lines oldest first. ts is the cross-file comparator; seq breaks
/// ties within a file (with one watched session, the shipped norm, this is
/// exactly the append order).
fn sort_lines(lines: &mut [LedgerLine]) {
    lines.sort_by(|a, b| (a.ts, a.seq, &a.id).cmp(&(b.ts, b.seq, &b.id)));
}

/// One message against the resolved filters. `with` means from or to.
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
    is_match: &dyn Fn(&LedgerLine) -> bool,
    consumed: &BTreeMap<String, u64>,
    limit: usize,
) -> (Vec<LedgerLine>, Option<BTreeMap<String, u64>>) {
    // Content: the merged fold, so every emitted line shows the current
    // per-recipient delivery states wherever they were recorded.
    let merged = merge_files(files);
    let content: HashMap<&str, &LedgerLine> = merged.iter().map(|l| (l.id.as_str(), l)).collect();

    // Every msg-line copy across every file, in global merge order.
    let mut copies: Vec<(u64, u64, &str, usize)> = Vec::new();
    for (fi, file) in files.iter().enumerate() {
        for l in file {
            if matches!(l.kind, Kind::Msg | Kind::Fyi) {
                copies.push((l.ts, l.seq, l.id.as_str(), fi));
            }
        }
    }
    copies.sort_by(|a, b| (a.0, a.1, a.2, a.3).cmp(&(b.0, b.1, b.2, b.3)));

    let mut next: BTreeMap<String, u64> = consumed.clone();
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

/// Assemble one thread: the folded msg line for `id`, every state/gate line
/// sharing `id`, and the folded msg line of every reply chaining to it.
/// None when the record has no line with that id at all.
pub(crate) fn thread_lines(files: &[Vec<LedgerLine>], id: &str) -> Option<Vec<LedgerLine>> {
    // Chain lines: state and gate lines carrying the id. Copies of the same
    // transition land in every session file hosting the delivery, so
    // cross-file duplicates collapse on content (everything but seq).
    let mut chain: Vec<LedgerLine> = Vec::new();
    let mut seen_chain: HashSet<String> = HashSet::new();
    let mut id_exists = false;
    for file in files {
        for line in file {
            if line.id != id {
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
    let msgs = merge_files(files);
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
    use cyclops_proto::{Delivery, DeliveryState, VerifiedBy};
    use std::path::Path;

    fn fixture() -> Vec<LedgerLine> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/history.ndjson");
        cyclops_ledger::read_after(&path, 0).expect("fixture reads")
    }

    fn folded() -> Vec<LedgerLine> {
        merge_files(&[fixture()])
    }

    fn ids(lines: &[LedgerLine]) -> Vec<&str> {
        lines.iter().map(|l| l.id.as_str()).collect()
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
        let open = open_from(&[fixture()]);
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

    fn walk_composite(files: &[Vec<LedgerLine>], names: &[String], limit: usize) -> Vec<String> {
        let mut cursor = BTreeMap::new();
        let mut walked = Vec::new();
        loop {
            let (batch, next) = page_composite(files, names, &all, &cursor, limit);
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
                walk_composite(&[a.clone(), b.clone()], &names, limit),
                vec!["m-a1", "m-b1", "m-a2", "m-b2"],
                "limit {limit}"
            );
        }
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
                walk_composite(&[a.clone(), b.clone()], &names, limit),
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
                walk_composite(&[a.clone(), b.clone()], &names, limit),
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
            &from_codex,
            &BTreeMap::new(),
            1,
        );
        assert_eq!(batch[0].id, "m-yes1");
        let (batch, next) = page_composite(&[a, b], &names, &from_codex, &next.unwrap(), 1);
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
        let lines = thread_lines(&files, "m-aaaaaa").expect("thread exists");

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
        let lines = thread_lines(&files, "m-cccccc").expect("thread exists");
        assert!(lines.iter().any(|l| l.id == "m-eeeeee"), "descendant");
        assert!(
            !lines.iter().any(|l| l.id == "m-aaaaaa"),
            "the brief scopes threads to descendants"
        );
    }

    #[test]
    fn thread_unknown_id_is_none() {
        assert!(thread_lines(&[fixture()], "m-nope00").is_none());
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
        let merged = merge_files(&[file_a, file_b]);
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
}
