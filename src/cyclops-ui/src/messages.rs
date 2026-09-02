//! One authenticated messages snapshot, turned into queue rows.
//!
//! The wire answer is per message and carries a recipient list. The queue
//! is per message AND recipient, because that is the granularity a person
//! acts at: a broadcast to three agents is three pieces of work. This
//! module is the only place that fan-out happens.
//!
//! It also holds the refresh gate. `messages.changed` is only a wake edge:
//! the queue still comes from one whole authenticated snapshot, never from
//! patches reconstructed by this client.

use cyclops_proto::{
    MailboxEntryState, MessageDirection, MessageNotificationSettlement, MessageNotificationState,
    MessageQuotaState, MessageSnapshotRow, MessagesChangedData, MessagesFollowResult,
    MessagesSnapshotResult, RecipientKey, WorkspaceId,
};

use crate::queue::{Direction, MailboxWord, QueueRow, QueueTarget, Snapshot, WakeWord};
use crate::stream::{Entry, EntryKind, MessageEndpoints};

const FOLLOW_PAGE_LIMIT: u32 = 128;

/// One generation-stamped, bounded follow request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowRequest {
    generation: u64,
    request: u64,
    after_seq: u64,
    limit: u32,
}

impl FollowRequest {
    pub fn after_seq(self) -> u64 {
        self.after_seq
    }

    pub fn limit(self) -> u32 {
        self.limit
    }
}

/// Turns pushed invalidations plus authenticated cursor pages into new
/// body-free stream entries. Queue snapshots establish the baseline but
/// never advance the cursor after that: their settled tail is bounded.
#[derive(Debug, Default)]
pub struct MessageFollower {
    workspace_id: Option<WorkspaceId>,
    cursor: Option<u64>,
    first_unseen_edge: Option<u64>,
    target_seq: Option<u64>,
    in_flight: Option<FollowRequest>,
    generation: u64,
    next_request: u64,
    faulted: bool,
}

impl MessageFollower {
    pub fn connected(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.in_flight = None;
        self.faulted = false;
    }

    pub fn disconnected(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.in_flight = None;
    }

    pub fn changed(&mut self, changed: &MessagesChangedData) {
        if self
            .workspace_id
            .is_some_and(|id| id != changed.workspace_id)
        {
            self.cursor = None;
            self.first_unseen_edge = None;
            self.target_seq = None;
            self.in_flight = None;
        }
        self.workspace_id = Some(changed.workspace_id);
        if self.cursor.is_none() {
            self.first_unseen_edge = Some(
                self.first_unseen_edge
                    .map_or(changed.workspace_seq, |seq| seq.min(changed.workspace_seq)),
            );
        }
        self.target_seq = Some(
            self.target_seq
                .map_or(changed.workspace_seq, |seq| seq.max(changed.workspace_seq)),
        );
    }

    /// Establish or refresh the workspace head from one accepted queue
    /// snapshot. Only the first snapshot may set the cursor directly.
    pub fn baseline(&mut self, snapshot: &MessagesSnapshotResult) {
        if self
            .workspace_id
            .is_some_and(|id| id != snapshot.workspace_id)
        {
            self.cursor = None;
            self.first_unseen_edge = None;
            self.target_seq = None;
            self.in_flight = None;
        }
        self.workspace_id = Some(snapshot.workspace_id);
        if self.cursor.is_none() {
            self.cursor = Some(
                self.first_unseen_edge
                    .map_or(snapshot.workspace_seq, |seq| seq.saturating_sub(1)),
            );
        }
        self.target_seq = Some(self.target_seq.map_or(snapshot.workspace_seq, |seq| {
            seq.max(snapshot.workspace_seq)
        }));
        self.first_unseen_edge = None;
    }

    pub fn begin(&mut self) -> Option<FollowRequest> {
        let cursor = self.cursor?;
        let target = self.target_seq?;
        if self.faulted || self.in_flight.is_some() || cursor >= target {
            return None;
        }
        self.next_request = self.next_request.wrapping_add(1);
        let request = FollowRequest {
            generation: self.generation,
            request: self.next_request,
            after_seq: cursor,
            limit: FOLLOW_PAGE_LIMIT,
        };
        self.in_flight = Some(request);
        Some(request)
    }

    /// Apply one exact page. The cursor advances only to the sequence the
    /// server says this page fully covered, never to a bounded queue head.
    pub fn finish(
        &mut self,
        request: FollowRequest,
        page: &MessagesFollowResult,
    ) -> Result<Vec<Entry>, &'static str> {
        if self.in_flight != Some(request) {
            return Ok(Vec::new());
        }
        self.in_flight = None;
        let rows_valid = page.rows.len() <= request.limit as usize
            && page
                .rows
                .iter()
                .try_fold(request.after_seq, |prior, row| {
                    (row.seq > prior && row.seq <= page.through_seq).then_some(row.seq)
                })
                .is_some();
        let page_end_valid =
            !page.has_more || page.rows.last().map(|row| row.seq) == Some(page.through_seq);
        if request.generation != self.generation
            || self.workspace_id != Some(page.workspace_id)
            || page.after_seq != request.after_seq
            || page.through_seq < request.after_seq
            || (page.has_more && page.through_seq == request.after_seq)
            || !rows_valid
            || !page_end_valid
        {
            self.faulted = true;
            return Err("invalid durable message cursor page");
        }
        self.cursor = Some(page.through_seq);
        self.target_seq = Some(
            self.target_seq
                .map_or(page.through_seq, |seq| seq.max(page.through_seq)),
        );
        Ok(page.rows.iter().map(stream_entry).collect())
    }

    pub fn failed(&mut self, request: FollowRequest) -> bool {
        if self.in_flight != Some(request) {
            return false;
        }
        self.in_flight = None;
        self.faulted = true;
        true
    }
}

fn stream_entry(row: &MessageSnapshotRow) -> Entry {
    let recipients = row.recipients.iter().map(|to| to.recipient).collect();
    let to = row
        .recipients
        .iter()
        .map(|recipient| {
            recipient
                .current_route
                .as_ref()
                .map(|route| route.label.clone())
                .unwrap_or_else(|| recipient.label.clone())
        })
        .collect();
    Entry {
        uid: 0,
        ts: row.ts,
        // Workspace and session ledgers have independent sequence domains.
        seq: None,
        id: Some(row.message_id.to_string()),
        kind: EntryKind::Msg {
            from: row.sender_label.clone(),
            to,
            endpoints: Some(MessageEndpoints {
                sender: row.sender,
                recipients,
            }),
            subject: row.subject.clone().unwrap_or_default(),
            fyi: row.kind == cyclops_proto::Kind::Fyi,
        },
    }
}

/// Fan one authenticated snapshot into queue rows.
///
/// Exactly one row per message and recipient. A recipient whose wake
/// attempt still needs a human is addressed by that attempt; everyone
/// else is addressed by the message and their own key. A message is never
/// emitted twice for one recipient, so an alarm and its message cannot
/// appear as two pieces of work.
pub fn rows_from_snapshot(snapshot: &MessagesSnapshotResult) -> Snapshot {
    let mut rows = Vec::new();
    for row in &snapshot.rows {
        for to in &row.recipients {
            rows.push(row_for(row, to.recipient, to));
        }
    }
    Snapshot {
        watermark: snapshot.workspace_seq,
        rows,
    }
}

fn row_for(
    row: &MessageSnapshotRow,
    recipient: RecipientKey,
    to: &cyclops_proto::MessageRecipientSummary,
) -> QueueRow {
    let wake = wake_word(&to.notification);
    let target = target_for(row, recipient);
    QueueRow {
        target,
        message_id: row.message_id.clone(),
        recipient,
        sender: row.sender,
        sender_label: crate::grid::safe_text(&row.sender_label),
        reply_to: row.reply_to.clone(),
        thread_root: row.thread_root.clone(),
        thread_message_count: row.thread_message_count,
        ts: row.ts,
        kind: row.kind,
        recipient_count: row.recipients.len(),
        // The exact attempt an attention action names, kept apart from
        // the identity precisely because it changes.
        attention: to.notification.attempt_id,
        resolution_intent: to.notification.resolution_intent,
        resolution_action_accepted: to.notification.resolution_action_accepted,
        resolution_consumption_observed: to.notification.resolution_consumption_observed,
        // The daemon's answer. Never inferred here: direction, mailbox
        // and wake are all visible to a client and none of them says who
        // is allowed to act.
        can_manage_attention: to.can_manage_attention,
        can_withdraw_notification: to.can_withdraw_notification,
        // Untrusted: both are written by whoever sent the message.
        recipient_label: crate::grid::safe_text(
            to.current_route
                .as_ref()
                .map(|route| route.label.as_str())
                .unwrap_or(&to.label),
        ),
        subject: row.subject.as_deref().map(crate::grid::safe_text),
        mailbox: mailbox_word(&to.mailbox),
        wake,
        cause: active_attention_cause(&to.notification, wake),
        pre_write_cause: to.notification.pre_write_cause,
        pre_write_block: to.notification.pre_write_block.clone(),
        wake_block: to.notification.wake_block,
        pane_width_block: to.notification.pane_width_block(),
        current_route: to.current_route.clone(),
        fifo_position: to.fifo_position,
        needs_action: to.needs_action,
        seq: row.seq,
        updated_at: to.notification.updated_at.unwrap_or(row.ts),
        direction: direction(to.direction),
    }
}

/// The current hold cause, if this wake is still asking someone to act.
///
/// Notification summaries retain an original after-write cause after a later
/// resolution so the ledger can explain the attempt. The queue is a current
/// work surface: showing that historical cause next to a submitted, discarded,
/// or cleared wake makes a completed recovery look like an active hold.
fn active_attention_cause(
    notification: &cyclops_proto::MessageNotificationSummary,
    wake: WakeWord,
) -> Option<cyclops_proto::NotificationAttentionCause> {
    match wake {
        WakeWord::NeedsAttention | WakeWord::ResolutionIncomplete => notification.cause,
        _ => None,
    }
}

/// Identity is the row, for the row's whole life.
///
/// It used to be "message, unless an alarm is open, then the attempt",
/// which made an identity that changed as alarms appeared and cleared.
/// The cursor and every open detail key on it, so a lifecycle change lost
/// the cursor's row and left a detail stale the moment its own action
/// succeeded. The attempt now rides alongside as the exact thing an
/// action names.
fn target_for(row: &MessageSnapshotRow, recipient: RecipientKey) -> QueueTarget {
    QueueTarget::new(row.message_id.clone(), recipient)
}

fn mailbox_word(state: &MailboxEntryState) -> MailboxWord {
    match state {
        MailboxEntryState::Pending => MailboxWord::Pending,
        MailboxEntryState::Claimed { .. } => MailboxWord::Claimed,
        MailboxEntryState::DeliveredDirect { .. } => MailboxWord::DeliveredDirect,
        MailboxEntryState::Superseded { .. } => MailboxWord::Superseded,
    }
}

/// The wire wake state and additive settlement in the words a reader needs.
///
/// Every durable phase stays distinct. A staged notification and a queued
/// notification demand different operator conclusions even though neither
/// proves an agent acknowledgement. An alarm an operator has already
/// acknowledged reads as cleared rather than as still needing them.
fn wake_word(n: &cyclops_proto::MessageNotificationSummary) -> WakeWord {
    if n.operator_withdrawn == Some(true) {
        return WakeWord::WithdrawnByOperator;
    }
    if n.settlement == Some(MessageNotificationSettlement::WithdrawnByClaim) {
        return WakeWord::Withdrawn;
    }
    if n.wake_block.is_some() || n.pre_write_cause.is_some() {
        return WakeWord::BlockedBeforeWrite;
    }
    match n.quota_state {
        Some(MessageQuotaState::Held) => return WakeWord::QuotaHeld,
        Some(MessageQuotaState::ResetObserved) => return WakeWord::QuotaResetObserved,
        None => {}
    }
    match n.resolution {
        Some(cyclops_proto::NotificationResolution::Complete) => {
            return WakeWord::ResolvedSubmitted;
        }
        Some(cyclops_proto::NotificationResolution::Discard) => {
            return WakeWord::ResolvedDiscarded;
        }
        None => {}
    }
    match n.state {
        MessageNotificationState::NotStarted => WakeWord::NotStarted,
        MessageNotificationState::Queued => WakeWord::Queued,
        MessageNotificationState::Gating => WakeWord::Gating,
        MessageNotificationState::Writing => WakeWord::Writing,
        MessageNotificationState::Staged => WakeWord::Staged,
        MessageNotificationState::Submitted => WakeWord::Submitted,
        MessageNotificationState::SubmittedUnverified => WakeWord::SubmittedUnverified,
        MessageNotificationState::Notified => WakeWord::Notified,
        MessageNotificationState::AttentionRequired => {
            if (n.resolution_intent.is_some()
                || n.resolution_action_accepted.is_some()
                || n.resolution_consumption_observed.is_some())
                && n.resolution.is_none()
            {
                WakeWord::ResolutionIncomplete
            } else if n.attention_cleared == Some(true) {
                WakeWord::Cleared
            } else {
                WakeWord::NeedsAttention
            }
        }
        MessageNotificationState::Superseded => WakeWord::Superseded,
    }
}

/// Caller-relative, and read per mailbox rather than per message.
///
/// The message-level pair answers once for the whole message. Copying it
/// onto every fanned row puts other recipients' mail in the caller's
/// inbox, and makes one recipient's alarm look like work on all of them.
///
/// A message addressed to yourself is inbound. A mailbox you neither
/// sent to nor own is observed, which keeps other people's mail out of
/// your scopes even when you can see it.
fn direction(d: MessageDirection) -> Direction {
    match d {
        MessageDirection::Inbound | MessageDirection::SelfAddressed => Direction::Inbound,
        MessageDirection::Outbound => Direction::Outbound,
        MessageDirection::Workspace => Direction::Observed,
    }
}

/// Connection state and whole-snapshot freshness.
///
/// The contract is push invalidation plus whole-snapshot replacement:
/// one fetch in flight at a time, an edge arriving during a fetch causes
/// exactly one follow-up rather than a fetch per edge, and a reconnect
/// replaces everything before later edges are believed.
///
/// Nothing here reads a clock. There is no timer and no poll: a fetch
/// happens because something said the state changed, or because the
/// connection came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshRequest {
    generation: u64,
    request: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshGate {
    dirty: bool,
    in_flight: Option<RefreshRequest>,
    link: Link,
    generation: u64,
    next_request: u64,
    workspace_id: Option<WorkspaceId>,
    workspace_seq: Option<u64>,
    snapshot_current: bool,
}

/// Where the daemon connection stands.
///
/// Connecting and Lost are separate so one reconnect can be in flight at
/// a time. Connected still does not authorize mutations until a snapshot
/// from that connection generation lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Link {
    /// Startup or an explicit reconnect attempt. Nothing is acknowledged.
    #[default]
    Connecting,
    /// The subscription is acknowledged. Only now can a later change be
    /// guaranteed not to be missed.
    Connected,
    /// A connection attempt failed or an acknowledged subscription ended.
    /// Any snapshot on screen may be stale.
    Lost,
}

impl RefreshGate {
    pub fn new() -> RefreshGate {
        RefreshGate::default()
    }

    pub fn link(&self) -> Link {
        self.link
    }

    /// May a mutation be sent right now?
    ///
    /// Only against an acknowledged connection and a current snapshot.
    /// A request written into a socket that is on its way down, or based
    /// on facts from before a reconnect, is not safe to present as live.
    pub fn may_mutate(&self) -> bool {
        self.link == Link::Connected && self.snapshot_current
    }

    /// Something outside the workspace journal changed, such as route
    /// availability or an action this operator just completed.
    ///
    /// Invalidates the generation, so an answer already in flight is
    /// refused rather than merely followed by another fetch. Without
    /// that, the reply to a request made BEFORE the change could still
    /// land and be believed, leaving one window in which the surface
    /// showed pre-change state and offered actions against it.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        self.invalidate_generation();
    }

    /// A durable messaging fact changed.
    ///
    /// Duplicate or older edges are ignored. A gap invalidates the
    /// current request even if its answer later looks plausible: only a
    /// request started after the gap may replace the queue.
    /// Returns true when the caller must expose that gap while rebuilding.
    pub fn messages_changed(&mut self, changed: &MessagesChangedData) -> bool {
        let mut gap = false;
        let newer = match (self.workspace_id, self.workspace_seq) {
            (Some(workspace_id), _) if workspace_id != changed.workspace_id => {
                self.invalidate_generation();
                gap = true;
                true
            }
            (_, Some(workspace_seq)) if changed.workspace_seq <= workspace_seq => false,
            (_, Some(workspace_seq)) => {
                if changed.workspace_seq > workspace_seq.saturating_add(1) {
                    self.invalidate_generation();
                    gap = true;
                }
                true
            }
            _ => true,
        };
        if !newer {
            return false;
        }
        self.workspace_id = Some(changed.workspace_id);
        self.workspace_seq = Some(changed.workspace_seq);
        self.dirty = true;
        self.snapshot_current = false;
        gap
    }

    /// The connection came back. Everything on screen predates the gap,
    /// so a whole snapshot has to land before any later edge is trusted.
    pub fn connected(&mut self) {
        self.link = Link::Connected;
        self.snapshot_current = false;
        self.dirty = true;
        self.invalidate_generation();
        self.in_flight = None;
        self.workspace_id = None;
        self.workspace_seq = None;
    }

    pub fn disconnected(&mut self) {
        // The connection attempt ended. Lost offers an explicit retry;
        // Connecting would make a failed first attempt unrecoverable.
        self.link = Link::Lost;
        self.snapshot_current = false;
        self.invalidate_generation();
        self.in_flight = None;
    }

    pub fn is_connected(&self) -> bool {
        self.link == Link::Connected
    }

    /// Begin one operator-requested reconnect.
    ///
    /// Moving to Connecting before the runtime opens the socket makes
    /// repeated requests no-ops until that attempt succeeds or fails.
    pub fn reconnecting(&mut self) -> bool {
        if self.link != Link::Lost {
            return false;
        }
        self.link = Link::Connecting;
        self.snapshot_current = false;
        true
    }

    /// Start a fetch if one is owed and none is running.
    pub fn begin(&mut self) -> Option<RefreshRequest> {
        if self.link != Link::Connected || !self.dirty || self.in_flight.is_some() {
            return None;
        }
        self.snapshot_current = false;
        self.dirty = false;
        self.next_request = self.next_request.wrapping_add(1);
        let request = RefreshRequest {
            generation: self.generation,
            request: self.next_request,
        };
        self.in_flight = Some(request);
        Some(request)
    }

    /// Accept a snapshot only for the current request and event horizon.
    pub fn finish_snapshot(
        &mut self,
        request: RefreshRequest,
        snapshot: &MessagesSnapshotResult,
    ) -> bool {
        if self.in_flight != Some(request) {
            return false;
        }
        self.in_flight = None;
        if request.generation != self.generation
            || self
                .workspace_id
                .is_some_and(|workspace_id| workspace_id != snapshot.workspace_id)
            || self
                .workspace_seq
                .is_some_and(|workspace_seq| snapshot.workspace_seq < workspace_seq)
        {
            self.dirty = true;
            return false;
        }
        self.workspace_id = Some(snapshot.workspace_id);
        self.workspace_seq = Some(snapshot.workspace_seq);
        self.snapshot_current = true;
        true
    }

    /// Finish a failed request. A stale failure belongs to an invalidated
    /// connection generation and must not replace a newer notice.
    pub fn finish_failure(&mut self, request: RefreshRequest) -> bool {
        if self.in_flight != Some(request) {
            return false;
        }
        self.in_flight = None;
        if request.generation != self.generation {
            return false;
        }
        // The last snapshot remains useful but is no longer current.
        // Move to Lost so clients whose snapshot shares the subscription
        // reconnect explicitly instead of retrying forever.
        self.link = Link::Lost;
        self.snapshot_current = false;
        true
    }

    /// Finish a failed one-shot snapshot while the caller's independent
    /// event subscription remains acknowledged.
    pub fn finish_snapshot_failure(&mut self, request: RefreshRequest) -> bool {
        if self.in_flight != Some(request) {
            return false;
        }
        self.in_flight = None;
        if request.generation != self.generation {
            return false;
        }
        self.snapshot_current = false;
        true
    }

    pub fn is_fetching(&self) -> bool {
        self.in_flight.is_some()
    }

    fn invalidate_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.snapshot_current = false;
    }
}
