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
    MailboxEntryState, MessageDirection, MessageNotificationState, MessageQuotaState,
    MessageSnapshotRow, MessagesChangedData, MessagesSnapshotResult, RecipientKey, WorkspaceId,
};

use crate::queue::{Direction, MailboxWord, QueueRow, QueueTarget, Snapshot, WakeWord};

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
        // The exact attempt an attention action names, kept apart from
        // the identity precisely because it changes.
        attention: to.notification.attempt_id,
        // The daemon's answer. Never inferred here: direction, mailbox
        // and wake are all visible to a client and none of them says who
        // is allowed to act.
        can_manage_attention: to.can_manage_attention,
        // Untrusted: both are written by whoever sent the message.
        recipient_label: crate::grid::safe_text(&to.label),
        subject: row.subject.as_deref().map(crate::grid::safe_text),
        mailbox: mailbox_word(&to.mailbox),
        wake,
        cause: to.notification.cause,
        needs_action: to.needs_action,
        seq: row.seq,
        updated_at: to.notification.updated_at.unwrap_or(row.ts),
        direction: direction(to.direction),
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

/// The wire's nine wake states in the five words a reader needs.
///
/// Everything between queued and submitted is one thing to a person:
/// an attempt is in flight and has not been acknowledged. `NotStarted`
/// stays on its own, because no attempt at all is a different situation
/// and it changes whether requeue is the right action. An alarm an
/// operator has already acknowledged reads as cleared rather than as
/// still needing them.
fn wake_word(n: &cyclops_proto::MessageNotificationSummary) -> WakeWord {
    match n.quota_state {
        Some(MessageQuotaState::Held) => return WakeWord::QuotaHeld,
        Some(MessageQuotaState::ResetObserved) => return WakeWord::QuotaResetObserved,
        None => {}
    }
    match n.resolution {
        Some(cyclops_proto::NotificationResolution::Complete) => {
            return WakeWord::OperatorSubmitted;
        }
        Some(cyclops_proto::NotificationResolution::Discard) => {
            return WakeWord::OperatorDiscarded;
        }
        None => {}
    }
    match n.state {
        MessageNotificationState::NotStarted => WakeWord::NotStarted,
        MessageNotificationState::Queued
        | MessageNotificationState::Gating
        | MessageNotificationState::Writing
        | MessageNotificationState::Staged
        | MessageNotificationState::Submitted => WakeWord::Waiting,
        MessageNotificationState::Notified => WakeWord::Notified,
        MessageNotificationState::AttentionRequired => {
            if n.resolution_intent.is_some() && n.resolution.is_none() {
                WakeWord::ActionUncertain
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
    pub fn messages_changed(&mut self, changed: &MessagesChangedData) {
        let newer = match (self.workspace_id, self.workspace_seq) {
            (Some(workspace_id), _) if workspace_id != changed.workspace_id => {
                self.invalidate_generation();
                true
            }
            (_, Some(workspace_seq)) if changed.workspace_seq <= workspace_seq => false,
            (_, Some(workspace_seq)) => {
                if changed.workspace_seq > workspace_seq.saturating_add(1) {
                    self.invalidate_generation();
                }
                true
            }
            _ => true,
        };
        if !newer {
            return;
        }
        self.workspace_id = Some(changed.workspace_id);
        self.workspace_seq = Some(changed.workspace_seq);
        self.dirty = true;
        self.snapshot_current = false;
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
        // Move to Lost so the operator gets one explicit R recovery path
        // instead of an automatic retry loop or a permanently disabled UI.
        self.link = Link::Lost;
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
