//! The open detail: one frozen target, what was loaded for it, and the
//! actions it allows.
//!
//! Pure. Nothing here opens a socket or performs an action. It decides
//! what may happen and records what did; [`crate::action_io`] does it.
//!
//! The one rule the whole module exists to keep: a detail is opened
//! against one target frozen at that moment, and nothing that arrives
//! afterwards moves it. A snapshot replacement can empty the list under
//! an open confirmation and the confirmation still names what the
//! operator read.

use cyclops_proto::{MessageRecipientRoute, NotificationPreWriteCause, NotificationResolution};

use crate::queue::{Direction, FrozenTarget, MailboxWord, QueueRow, WakeWord};

/// What a detail can be asked to do.
///
/// Closed set. A renderer offers only what [`Detail::allowed`] returns,
/// and the daemon rechecks everything at mutation time regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Reply,
    /// Retire one exact wake that the daemon proved never wrote terminal bytes.
    WithdrawNotification,
    /// Acknowledge an alarm.
    ClearAlarm,
    AttentionComplete,
    AttentionDiscard,
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Action::Reply => "reply",
            Action::WithdrawNotification => "withdraw pre-write wake",
            Action::ClearAlarm => "clear alarm",
            Action::AttentionComplete => "submit staged notification",
            Action::AttentionDiscard => "discard staged notification",
        }
    }

    /// Does this action need the operator to say yes to a named target?
    ///
    /// Anything that changes what an agent sees, or ends an alarm's life,
    /// is confirmed. Claim and reply are the operator's own reading and
    /// writing and are not.
    pub fn needs_confirmation(self) -> bool {
        !matches!(self, Action::Reply)
    }
}

/// One diagnostic check the daemon reported for an attention target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: Option<String>,
}

/// One earlier message in the thread. Metadata plus the body the daemon
/// chose to authorize, which is absent for anything the caller may not
/// read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadEntry {
    pub message_id: String,
    pub sender_label: String,
    pub subject: Option<String>,
    pub body: Option<String>,
    pub ts: u64,
}

/// What the daemon returned for the frozen target.
///
/// A body appears only when the daemon put one here. The UI never
/// decides that a body may be shown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub body: Option<String>,
    /// The daemon authorized this reader to read the message, including
    /// when the message legitimately has no body.
    pub body_authorized: bool,
    pub thread: Vec<ThreadEntry>,
    pub checks: Vec<Check>,
    /// Said out loud in the detail so a reader knows which happened.
    pub claim_note: Option<String>,
    /// Why the thread is missing, when the claim landed and the context
    /// did not. Absent when there is nothing to explain.
    pub thread_note: Option<String>,
    /// The notification row the daemon staged for this attempt.
    ///
    /// Only ever what `attention.show` returned. The daemon says this is a
    /// content-free doorbell and never the durable subject or body, so
    /// showing it discloses nothing the operator could not already act on.
    pub expected: Option<String>,
    /// What was actually extracted from the pane, when extraction worked.
    ///
    /// Absent is information: it means the daemon could not read the pane
    /// exactly, which is itself a reason a check failed. Never synthesized
    /// here, because a client-invented "observed" would be this crate
    /// giving a second opinion on evidence it did not gather.
    pub observed: Option<String>,
}

/// A reply being written.
///
/// The key is minted once for a set of bytes and reused for every retry
/// of those bytes, so a send that timed out after the daemon accepted it
/// cannot become two messages. Editing the text after a failure is a
/// different message and takes a new key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    text: String,
    key: Option<String>,
    keyed_for: Option<String>,
}

impl Draft {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }

    pub fn push(&mut self, ch: char) {
        self.text.push(ch);
    }

    pub fn backspace(&mut self) {
        self.text.pop();
    }

    /// The key these exact bytes send under, minting one if needed.
    ///
    /// `mint` supplies the identifier so the caller owns randomness and
    /// this module stays pure and testable.
    pub fn key_for_send(&mut self, mint: impl FnOnce() -> String) -> String {
        if self.keyed_for.as_deref() != Some(self.text.as_str()) {
            self.key = Some(mint());
            self.keyed_for = Some(self.text.clone());
        }
        self.key.clone().expect("just set")
    }

    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

/// Where an open detail stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// A read is in flight for the frozen target. Enter does nothing
    /// here, which is what stops a second press opening a second one.
    Opening,
    Open,
    /// Awaiting an explicit yes for one named action.
    Confirming(Action),
    /// A confirmed action is in flight.
    Acting(Action),
    /// The daemon refused. It answered, so the operator knows where
    /// they stand and the detail stays usable.
    Failed {
        action: Option<Action>,
        why: String,
    },
    /// The request never left this process, so nothing happened.
    ///
    /// A connect or hello failure is knowledge, not doubt: everything
    /// stays available because the daemon never saw it.
    NotSent {
        action: Option<Action>,
        why: String,
    },
    /// The request was sent and its outcome is unknown.
    ///
    /// Only the two terminal verbs are withheld after this, and only
    /// when the uncertain action was one of them. Reply carries an
    /// idempotency key and clearing an alarm is idempotent by design, so
    /// blocking those would strand an operator over somebody else's
    /// ambiguity.
    ///
    /// The action is optional: an open that claims is a mutation, and a
    /// claim whose answer never arrived is unknown rather than absent.
    Uncertain {
        action: Option<Action>,
        why: String,
    },
}

/// What backing out did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Back {
    /// A confirmation was dropped. Nothing was sent.
    Cancelled,
    /// The detail closed.
    Closed,
}

/// One open detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    target: FrozenTarget,
    /// Row facts copied at open, so the detail can describe itself after
    /// the row leaves the list.
    message_id: String,
    recipient_label: String,
    subject: Option<String>,
    direction: Direction,
    mailbox: MailboxWord,
    wake: WakeWord,
    stage: Stage,
    loaded: Loaded,
    draft: Draft,
    /// The row is gone from the latest snapshot. Said out loud rather
    /// than acted on: what the operator froze is still what they froze.
    stale: bool,
    /// The row is still here but its facts moved, so what was read for
    /// it is out of date. The target does not change; the read does.
    ///
    /// Cleared on failure as well as on success, because the loop re-takes
    /// whatever is owed every turn and a failing read would spin.
    needs_reload: bool,
    /// The evidence on screen was measured against facts that have since
    /// moved, and no successful reload has replaced it.
    ///
    /// Separate from `needs_reload` on purpose. That one answers "should
    /// I ask again", and has to be cleared when a read fails or the loop
    /// spins. This one answers "may I act on what I am showing", and a
    /// failed reload must NOT clear it: doing so would declare stale
    /// evidence current because the retry did not work.
    evidence_stale: bool,
    /// First body row drawn. A body longer than the frame is scrolled,
    /// not truncated.
    scroll: usize,
    /// This attempt has been resolved, by this operator or another one.
    /// Terminal actions never come back.
    resolved: bool,
    /// A terminal verb was attempted and its outcome is unknown. Fresh
    /// terminal actions stay blocked. Intent alone exposes no action.
    terminal_unknown: bool,
    /// Exact durable action intended before any terminal key.
    resolution_intent: Option<NotificationResolution>,
    /// Exact durable action key the terminal accepted.
    resolution_action_accepted: Option<NotificationResolution>,
    /// Durable proof that an accepted Complete action started its turn.
    resolution_consumption_observed:
        Option<cyclops_proto::NotificationResolutionConsumptionObservation>,
    /// The reader is typing a reply. Keys go to the draft while it is on.
    composing: bool,
    /// The daemon's answer, frozen with the row: may THIS reader start a
    /// fresh resolution for THIS recipient's alarm. Never inferred from
    /// anything on screen. Matching durable-intent reconciliation is separate.
    can_manage_attention: bool,
    /// The daemon's answer for this exact unwritten wake and authenticated caller.
    can_withdraw_notification: bool,
    pre_write_cause: Option<NotificationPreWriteCause>,
    current_route: Option<MessageRecipientRoute>,
    fifo_position: Option<u64>,
}

/// Has this attempt already reached a terminal outcome?
///
/// The snapshot is the only place this can come from. `attention.show`
/// returns evidence, not disposition, so a detail that asked only the
/// daemon would offer Complete and Discard on an attempt another
/// operator finished minutes ago. The daemon would refuse, but offering
/// an action that cannot succeed is the defect.
fn resolved_from(wake: WakeWord) -> bool {
    matches!(
        wake,
        WakeWord::ResolvedSubmitted | WakeWord::ResolvedDiscarded
    )
}

/// Did a terminal action cross its write boundary without an outcome?
///
/// Distinct from resolved: nobody knows what happened, so fresh terminal
/// actions stay blocked. Matching durable intent and terminal acceptance
/// may expose only exact same-action no-key reconciliation.
fn terminal_unknown_from(wake: WakeWord) -> bool {
    matches!(wake, WakeWord::ResolutionIncomplete)
}

impl Detail {
    /// Open against one row, freezing its target and the watermark.
    pub fn open(row: &QueueRow, watermark: u64) -> Detail {
        Detail {
            target: FrozenTarget {
                target: row.target.clone(),
                attempt: row.attention,
                watermark,
            },
            can_manage_attention: row.can_manage_attention,
            can_withdraw_notification: row.can_withdraw_notification,
            pre_write_cause: row.pre_write_cause,
            current_route: row.current_route.clone(),
            fifo_position: row.fifo_position,
            message_id: row.message_id.to_string(),
            recipient_label: row.recipient_label.clone(),
            subject: row.subject.clone(),
            direction: row.direction,
            mailbox: row.mailbox,
            wake: row.wake,
            stage: Stage::Opening,
            loaded: Loaded::default(),
            draft: Draft::default(),
            stale: false,
            needs_reload: false,
            evidence_stale: false,
            scroll: 0,
            resolved: resolved_from(row.wake),
            terminal_unknown: terminal_unknown_from(row.wake),
            resolution_intent: row.resolution_intent,
            resolution_action_accepted: row.resolution_action_accepted,
            resolution_consumption_observed: row.resolution_consumption_observed,
            composing: false,
        }
    }

    pub fn target(&self) -> &FrozenTarget {
        &self.target
    }

    pub fn stage(&self) -> &Stage {
        &self.stage
    }

    pub fn loaded(&self) -> &Loaded {
        &self.loaded
    }

    pub fn draft(&self) -> &Draft {
        &self.draft
    }

    pub fn draft_mut(&mut self) -> &mut Draft {
        &mut self.draft
    }

    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    pub fn recipient_label(&self) -> &str {
        &self.recipient_label
    }

    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// The read that opened this detail came back.
    pub fn loaded_ok(&mut self, mut loaded: Loaded) {
        loaded.body_authorized |= loaded.body.is_some()
            || self.direction == Direction::Outbound
            || (self.direction == Direction::Inbound
                && matches!(
                    self.mailbox,
                    MailboxWord::Claimed | MailboxWord::DeliveredDirect
                ));
        self.loaded = loaded;
        self.stage = Stage::Open;
        self.needs_reload = false;
        // The only thing that makes evidence current again.
        self.evidence_stale = false;
    }

    /// A read or an action failed. The detail stays open and keeps the
    /// draft: losing what somebody typed because a socket blinked is the
    /// failure this guards against.
    pub fn failed(&mut self, action: Option<Action>, why: impl Into<String>) {
        self.needs_reload = false;
        self.stage = Stage::Failed {
            action,
            why: why.into(),
        };
    }

    /// The attempt did not complete and its outcome is unknown.
    ///
    /// The action is optional because a read can be uncertain too: an
    /// open that claims is a mutation, so a claim whose answer never
    /// arrived must be recorded as unknown rather than as nothing having
    /// happened. The draft is kept, because a reply retried under the
    /// same key is safe.
    pub fn uncertain(&mut self, action: Option<Action>, why: impl Into<String>) {
        // Only a terminal verb poisons the terminal pair. A reply, a
        // clear, or a claiming read that went unanswered says nothing
        // about whether this attempt was resolved.
        if matches!(
            action,
            Some(Action::AttentionComplete) | Some(Action::AttentionDiscard)
        ) {
            self.terminal_unknown = true;
        }
        if action == Some(Action::WithdrawNotification) {
            // The exact withdrawal is idempotent, but a timeout still owes a
            // fresh snapshot before the UI offers any mutation again.
            self.can_withdraw_notification = false;
        }
        self.needs_reload = false;
        self.stage = Stage::Uncertain {
            action,
            why: why.into(),
        };
    }

    /// The request never reached the daemon. Nothing happened, so
    /// nothing is withheld.
    pub fn not_sent(&mut self, action: Option<Action>, why: impl Into<String>) {
        self.needs_reload = false;
        self.stage = Stage::NotSent {
            action,
            why: why.into(),
        };
    }

    /// The daemon refused. A refusal that says the attempt is already
    /// resolved retires both terminal verbs for good: they are not
    /// repeatable and the state they act on is gone.
    pub fn refused(&mut self, action: Option<Action>, code: &str, message: impl Into<String>) {
        self.needs_reload = false;
        if code == "conflict" {
            self.resolved = true;
        }
        if action == Some(Action::WithdrawNotification) {
            self.can_withdraw_notification = false;
        }
        self.stage = Stage::Failed {
            action,
            why: message.into(),
        };
    }

    pub fn is_resolved(&self) -> bool {
        self.resolved
    }

    pub fn is_composing(&self) -> bool {
        self.composing
    }

    /// Start writing a reply. The draft that is already there stays, so
    /// a failed send can be reopened and sent again unchanged.
    pub fn begin_reply(&mut self) {
        self.composing = true;
        // Opening a composer is not acting. request() stamps Acting for
        // every Perform, and leaving it there while somebody types made
        // allowed() empty and, once Esc stopped closing a detail with a
        // live action, wedged it shut. Nothing is on the wire until the
        // draft is actually sent.
        self.stage = Stage::Open;
    }

    /// Stop writing without discarding what was written.
    pub fn end_compose(&mut self) {
        self.composing = false;
    }

    /// What the daemon says now. Never retargets: the frozen target and
    /// any open confirmation are exactly what the operator read.
    ///
    /// A row that is still listed but whose facts have moved leaves the
    /// detail owing a reload. The checks and states on screen were
    /// measured against the older facts, and acting on them would be
    /// acting on something that has changed underneath.
    pub fn observe_snapshot(&mut self, row: Option<&QueueRow>) {
        match row {
            None => self.stale = true,
            Some(row) => {
                self.stale = false;
                if row.mailbox != self.mailbox
                    || row.wake != self.wake
                    || row.attention != self.target.attempt
                    || row.resolution_intent != self.resolution_intent
                    || row.resolution_action_accepted != self.resolution_action_accepted
                    || row.resolution_consumption_observed != self.resolution_consumption_observed
                    || row.pre_write_cause != self.pre_write_cause
                    || row.current_route != self.current_route
                    || row.fifo_position != self.fifo_position
                {
                    self.mailbox = row.mailbox;
                    self.wake = row.wake;
                    self.resolution_intent = row.resolution_intent;
                    self.resolution_action_accepted = row.resolution_action_accepted;
                    self.resolution_consumption_observed = row.resolution_consumption_observed;
                    self.pre_write_cause = row.pre_write_cause;
                    self.current_route = row.current_route.clone();
                    self.fifo_position = row.fifo_position;
                    self.needs_reload = true;
                    self.evidence_stale = true;
                }
                // Another operator can resolve this attempt while it is
                // open here. Only ever tighten: a terminal verb this
                // operator already ran has retired the same two actions,
                // and a snapshot that predates it must not re-offer them.
                self.resolved |= resolved_from(row.wake);
                self.terminal_unknown |= terminal_unknown_from(row.wake);
                self.can_manage_attention &= row.can_manage_attention;
                self.can_withdraw_notification &= row.can_withdraw_notification;
            }
        }
    }

    /// Does this detail owe a fresh read? Cleared by the reload landing,
    /// and by a failure, so a failing read cannot spin.
    pub fn needs_reload(&self) -> bool {
        self.needs_reload
    }

    /// Is the evidence on screen older than the facts it describes?
    pub fn evidence_stale(&self) -> bool {
        self.evidence_stale
    }

    /// Ask for the reload again, by hand, after one failed.
    ///
    /// The explicit path that replaces retrying on its own: a read that
    /// failed stops being owed, so nothing happens until the operator
    /// says to try again.
    pub fn retry_reload(&mut self) {
        if self.evidence_stale {
            self.needs_reload = true;
        }
    }

    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn mailbox(&self) -> MailboxWord {
        self.mailbox
    }

    pub fn wake(&self) -> WakeWord {
        self.wake
    }

    pub fn resolution_intent(&self) -> Option<NotificationResolution> {
        self.resolution_intent
    }

    pub fn resolution_action_accepted(&self) -> Option<NotificationResolution> {
        self.resolution_action_accepted
    }

    pub fn resolution_consumption_observed(
        &self,
    ) -> Option<cyclops_proto::NotificationResolutionConsumptionObservation> {
        self.resolution_consumption_observed
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
    }

    /// Actions this detail may offer.
    ///
    /// Advisory only. The daemon rechecks authorization and evidence at
    /// mutation time, and a stale detail offers nothing at all.
    pub fn allowed(&self) -> Vec<Action> {
        if self.stale
            || !matches!(
                self.stage,
                Stage::Open
                    | Stage::Failed { .. }
                    | Stage::NotSent { .. }
                    | Stage::Uncertain { .. }
            )
        {
            return Vec::new();
        }
        if let Some(action) = self.reconciliation_action() {
            return if self.target.attempt.is_some() && !self.evidence_stale {
                vec![action]
            } else {
                Vec::new()
            };
        }
        let mut out = Vec::new();
        if self.can_withdraw_notification
            && self.target.attempt.is_some()
            && !self.evidence_stale
            && self.mailbox == MailboxWord::Pending
            && matches!(
                self.wake,
                WakeWord::Queued | WakeWord::Gating | WakeWord::BlockedBeforeWrite
            )
        {
            out.push(Action::WithdrawNotification);
        }
        // A row is a message that may ALSO carry an alarm. Both sets come
        // from one detail now, because the identity no longer decides
        // which kind of thing this is.
        // Evidence-gated actions need evidence that is current. A
        // snapshot can land in the same batch as a keypress, so without
        // this the reader could confirm against checks measured before
        // the facts moved.
        if self.can_manage_attention && self.target.attempt.is_some() && !self.evidence_stale {
            // Every check must pass before the two that mutate a pane are
            // even offered. The daemon repeats this.
            let checks_pass =
                !self.loaded.checks.is_empty() && self.loaded.checks.iter().all(|c| c.passed);
            if checks_pass && !self.resolved && !self.terminal_unknown {
                out.push(Action::AttentionComplete);
                out.push(Action::AttentionDiscard);
            }
            // Requeue is deliberately absent. msg.requeue acts on every
            // uncleared alarm of a message, so on a broadcast it reaches
            // recipients this confirmation never named. It stays in the
            // CLI until the verb can be scoped to one attempt.
            out.push(Action::ClearAlarm);
        }
        // No claim action. Opening an inbound pending row claims it, so a
        // second one here would be a button that either does nothing or
        // recovers the claim it just took.
        //
        // Reply needs two things. The body, because the daemon authorizes
        // that and this crate never decides it. And an inbound mailbox:
        // the daemon routes a reply to the parent's sender, so replying
        // on a row you SENT would address the message back to yourself. A
        // message you addressed to yourself arrives here as inbound and
        // keeps reply, which is deliberate: it is in your mailbox and the
        // reply lands in your own thread.
        if self.loaded.body_authorized && self.direction == Direction::Inbound {
            out.push(Action::Reply);
        }
        out
    }

    /// May this reader start a fresh resolution? The daemon's answer,
    /// frozen at open. Accepted-action reconciliation is separate.
    pub fn can_manage_attention(&self) -> bool {
        self.can_manage_attention
    }

    /// May this reader withdraw this exact unwritten wake?
    pub fn can_withdraw_notification(&self) -> bool {
        self.can_withdraw_notification
    }

    pub fn allows(&self, action: Action) -> bool {
        self.allowed().contains(&action)
    }

    /// Human copy for an action in this detail's current mode.
    pub fn action_word(&self, action: Action) -> &'static str {
        if self.reconciliation_action() == Some(action) {
            match action {
                Action::AttentionComplete => "reconcile prior uncertain submit",
                Action::AttentionDiscard if self.resolution_action_accepted.is_none() => {
                    "reconcile exact-empty discard without a key"
                }
                Action::AttentionDiscard => "reconcile prior uncertain discard",
                _ => unreachable!("only terminal actions have durable intents"),
            }
        } else {
            action.word()
        }
    }

    fn reconciliation_action(&self) -> Option<Action> {
        if self.wake != WakeWord::ResolutionIncomplete || self.resolved {
            return None;
        }
        let intent = self.resolution_intent?;
        match intent {
            NotificationResolution::Complete
                if self.resolution_action_accepted == Some(intent)
                    && self.resolution_consumption_observed.is_some() =>
            {
                Some(Action::AttentionComplete)
            }
            NotificationResolution::Discard
                if self.resolution_action_accepted.is_none()
                    || self.resolution_action_accepted == Some(intent) =>
            {
                Some(Action::AttentionDiscard)
            }
            _ => None,
        }
    }

    /// Ask for an action. Returns what the caller must do next.
    pub fn request(&mut self, action: Action) -> Request {
        if !self.allows(action) {
            return Request::Refused("not available for this item");
        }
        if action.needs_confirmation() {
            self.stage = Stage::Confirming(action);
            Request::Confirm(self.confirmation(action))
        } else {
            self.stage = Stage::Acting(action);
            Request::Perform(action)
        }
    }

    /// Say yes to the confirmation on screen.
    /// A mutation succeeded. A terminal verb that landed retires both.
    pub fn done(&mut self, action: Action, note: impl Into<String>) {
        if matches!(action, Action::AttentionComplete | Action::AttentionDiscard) {
            self.resolved = true;
        }
        // A reply that landed is not a draft any more. Keeping it left
        // the detail asserting both that the text was sent and that it
        // was still unsent, and worse, a second send of the same bytes
        // reused the same idempotency key: the daemon deduped it and the
        // operator was told a message went out that never did.
        if action == Action::Reply {
            self.draft = Draft::default();
        }
        if action == Action::WithdrawNotification {
            self.can_withdraw_notification = false;
            self.wake = WakeWord::WithdrawnByOperator;
            self.pre_write_cause = None;
        }
        self.loaded.claim_note = Some(note.into());
        self.stage = Stage::Open;
    }

    pub fn confirm(&mut self) -> Option<Action> {
        match self.stage {
            Stage::Confirming(action) => {
                self.stage = Stage::Acting(action);
                Some(action)
            }
            _ => None,
        }
    }

    /// Back out. Never mutates anything anywhere.
    pub fn escape(&mut self) -> Back {
        match self.stage {
            Stage::Confirming(_) => {
                self.stage = Stage::Open;
                Back::Cancelled
            }
            // A confirmed action is already on the wire and these verbs
            // are not idempotent. Closing here dropped the answer on its
            // token, so the detail never learned it had succeeded, the
            // queue was never re-read, and the row went on reading "needs
            // attention" until a reconnect. The operator's next move is to
            // reopen and inspect current durable state. Only a recorded
            // intent may expose matching no-key reconciliation. Esc waits.
            Stage::Acting(_) => Back::Cancelled,
            _ => Back::Closed,
        }
    }

    /// The sentence an operator says yes to. Names the exact target.
    pub fn confirmation(&self, action: Action) -> String {
        if self.reconciliation_action() == Some(action) {
            let attempt = self
                .target
                .attempt
                .expect("a reconciliation action requires one exact attempt");
            return format!(
                "{} for {} at seq {}; no second key will be sent. y to confirm, esc to cancel",
                self.action_word(action),
                attempt,
                self.target.watermark
            );
        }
        // The exact thing the action will name, which for the attention
        // verbs is the attempt and NOT the message. Once identity became
        // the row, naming the row here would have asked the operator to
        // say yes to a message id while the request carried an attempt
        // id they were never shown. A confirmation that names something
        // other than what it does is worse than no confirmation.
        let what = match (self.target.attempt, action) {
            (Some(attempt), Action::WithdrawNotification) => {
                return format!(
                    "{} {} for {} at seq {}? y to confirm, esc to cancel",
                    action.word(),
                    attempt,
                    self.target.target.recipient,
                    self.target.watermark
                );
            }
            (Some(attempt), Action::ClearAlarm)
            | (Some(attempt), Action::AttentionComplete)
            | (Some(attempt), Action::AttentionDiscard) => attempt.to_string(),
            _ => self.target.target.id(),
        };
        format!(
            "{} {} at seq {}? y to confirm, esc to cancel",
            self.action_word(action),
            what,
            self.target.watermark
        )
    }
}

/// What asking for an action produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Do it now.
    Perform(Action),
    /// Show this sentence and wait for a yes.
    Confirm(String),
    Refused(&'static str),
}

/// Cut or pad one line to an exact display width.
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = crate::grid::display_width(&ch.to_string());
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    while used < width {
        out.push(' ');
        used += 1;
    }
    out
}

/// Wrap text to a width without losing anything.
///
/// Public because that guarantee is the thing worth testing directly:
/// one line in, the pieces concatenated give back exactly what went in.
///
/// Character preserving. Every byte that comes in is drawn, including
/// leading indentation and runs of spaces, because a body can be a diff
/// or a snippet and reflowing it changes what it says. Lines break at
/// the last space that fits when there is one, and mid-token when there
/// is not, so a long url or hash is carried across rows rather than cut.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut used = 0usize;
        // Where the last space sits in `line`, for breaking politely.
        let mut break_at: Option<(usize, usize)> = None;
        for ch in para.chars() {
            let w = crate::grid::display_width(&ch.to_string());
            if used + w > width {
                match break_at.take() {
                    // Break at the space and carry the rest down.
                    // The break happens AFTER the space, so the space
                    // stays on the line it ended and nothing is dropped.
                    Some((byte, _)) if byte < line.len() => {
                        let rest: String = line[byte + 1..].to_string();
                        line.truncate(byte + 1);
                        out.push(std::mem::take(&mut line));
                        used = crate::grid::display_width(&rest);
                        line = rest;
                    }
                    _ => {
                        out.push(std::mem::take(&mut line));
                        used = 0;
                    }
                }
            }
            if ch == ' ' && used > 0 {
                break_at = Some((line.len(), used));
            }
            line.push(ch);
            used += w;
        }
        out.push(line);
    }
    out
}

/// Can a frame this size show the action list and a confirmation?
///
/// One predicate, consulted by the renderer and by the key handler, so
/// what the screen offers and what the keyboard accepts cannot disagree.
/// Below this the detail draws an identity and a way out and nothing
/// else, and a digit or a yes would be confirming something the reader
/// cannot read.
pub fn can_show_actions(width: usize, height: usize) -> bool {
    width >= CONFIRM_MIN_W && height >= CONFIRM_MIN_H
}

/// Enough columns for a confirmation to name its exact target.
///
/// A confirmation sentence carries a full attempt id, and `fit` cuts
/// rather than wraps, so at 24 columns the operator was asked to say yes
/// to a sentence that stopped before the id it was about. Saying yes to
/// a truncated target is the failure the confirmation exists to prevent.
const CONFIRM_MIN_W: usize = 48;

/// Enough rows to actually see the evidence.
///
/// The frame spends two rows on the header and two on the footer, so at
/// height 6 there are two body rows for five checks plus a staged and an
/// observed block. The terminal verbs are gated on every check passing,
/// and a reader who cannot see which check failed is confirming on the
/// UI's word rather than on evidence.
const CONFIRM_MIN_H: usize = 12;

/// Render the detail full width.
///
/// Pure: no colour, no cursor escape, no daemon. The body appears only
/// if the daemon put one in [`Loaded`], so this function has nothing to
/// leak when it was not authorized.
pub fn render(detail: &Detail, width: usize, height: usize) -> Vec<String> {
    render_with_status(detail, width, height, None)
}

pub(crate) fn render_with_status(
    detail: &Detail,
    width: usize,
    height: usize,
    status: Option<&str>,
) -> Vec<String> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let id = detail.target().target.id();
    let mut out: Vec<String> = Vec::new();

    // A sidebar cannot hold a detail. Say what is open and how to leave.
    let action_height = height.saturating_sub(usize::from(status.is_some()));
    if !can_show_actions(width, action_height) {
        out.push(fit(&tail(&id), width));
        out.push(fit(stage_word(detail.stage()), width));
        let reserved = 1 + usize::from(status.is_some());
        out.truncate(height.saturating_sub(reserved));
        while out.len() < height.saturating_sub(reserved) {
            out.push(fit("", width));
        }
        if let Some(status) = status {
            out.push(fit(status, width));
        }
        if out.len() < height {
            out.push(fit("esc back", width));
        }
        out.truncate(height);
        return out;
    }

    out.push(fit(&format!("{id}  {}", detail.recipient_label()), width));
    out.push(fit(
        &format!(
            "message {}  mailbox {}  wake {}  {}{}{}",
            detail.message_id(),
            detail.mailbox().cell(),
            detail.wake().cell(),
            stage_word(detail.stage()),
            if detail.is_stale() {
                "  (no longer listed)"
            } else {
                ""
            },
            if detail.needs_reload() {
                "  (changed, reopening)"
            } else {
                ""
            }
        ),
        width,
    ));
    out.push(fit(
        &format!("subject: {}", detail.subject().unwrap_or("(none)")),
        width,
    ));
    out.push(fit(&"-".repeat(width), width));

    let body_budget = height.saturating_sub(6);
    let mut body: Vec<String> = Vec::new();

    match detail.wake() {
        WakeWord::BlockedBeforeWrite => {
            let reason = match detail.pre_write_cause {
                Some(cause) => cause.label(),
                None => "reason unavailable",
            };
            body.push(format!("wake blocked before write: {reason}"));
            body.push(format!(
                "mailbox position: {}",
                detail
                    .fifo_position
                    .map(|position| position.to_string())
                    .unwrap_or_else(|| "unavailable".into())
            ));
            body.push(match &detail.current_route {
                Some(route) => format!("current route: {} ({})", route.label, route.pane_id),
                None => "current route: unavailable".into(),
            });
            body.push(String::new());
        }
        WakeWord::WithdrawnByOperator => {
            body.push("wake withdrawn by admin; message remains claimable".into());
            body.push(String::new());
        }
        WakeWord::QuotaHeld => {
            body.push(
                "quota held: wait for a quota reset; delivery will not resume automatically".into(),
            );
            body.push(String::new());
        }
        WakeWord::QuotaResetObserved => {
            body.push(format!(
                "quota reset observed: an administrator can run `cyclops requeue {}`",
                detail.message_id()
            ));
            body.push("this command requeues every eligible recipient on the message".into());
            body.push(String::new());
        }
        _ => {}
    }

    if let Some(intent) = detail.resolution_intent() {
        let action = match intent {
            NotificationResolution::Complete => "submit",
            NotificationResolution::Discard => "discard",
        };
        match detail.resolution_action_accepted() {
            Some(accepted) if accepted == intent => {
                if intent == NotificationResolution::Complete
                    && detail.resolution_consumption_observed().is_none()
                {
                    body.push("terminal accepted, task start unproven".into());
                    body.push("no submit or reconciliation action is available".into());
                } else {
                    body.push(format!(
                        "terminal accepted the {action} action; final outcome remains uncertain"
                    ));
                    if intent == NotificationResolution::Complete {
                        body.push("matching task start observed".into());
                    }
                    body.push(
                        "only same-action reconciliation is available; no second key is sent"
                            .into(),
                    );
                }
            }
            None => {
                body.push(format!(
                    "{action} intent recorded; terminal acceptance is unproven"
                ));
                if intent == NotificationResolution::Discard {
                    body.push("exact-empty no-key discard reconciliation is available".into());
                    body.push(
                        "the daemon rechecks the exact binding and empty composer twice; no terminal key is sent"
                            .into(),
                    );
                } else {
                    body.push("no submit, discard, or reconciliation action is available".into());
                }
            }
            Some(_) => {
                body.push("terminal intent and accepted action records disagree".into());
                body.push("no submit, discard, or reconciliation action is available".into());
            }
        }
        body.push(String::new());
    } else if detail.resolution_action_accepted().is_some() {
        body.push("terminal acceptance recorded without a matching intent".into());
        body.push("no submit, discard, or reconciliation action is available".into());
        body.push(String::new());
    }

    if let Some(note) = &detail.loaded().claim_note {
        body.push(note.clone());
        body.push(String::new());
    }
    for check in &detail.loaded().checks {
        let mark = if check.passed { "pass" } else { "FAIL" };
        body.push(format!("  {mark}  {}", check.name));
    }
    if !detail.loaded().checks.is_empty() {
        body.push(String::new());
    }
    // What the checks were measuring, for an operator about to complete
    // or discard. A failed check names which rule broke; this names what
    // actually differs, which is the part a person can act on.
    if detail.loaded().expected.is_some() || detail.loaded().observed.is_some() {
        body.push("staged".into());
        match &detail.loaded().expected {
            Some(text) => {
                for line in wrap(text, width.saturating_sub(2)) {
                    body.push(format!("  {line}"));
                }
            }
            None => body.push("  (the daemon returned no staged row)".into()),
        }
        body.push(String::new());
        body.push("in the pane".into());
        match &detail.loaded().observed {
            Some(text) => {
                for line in wrap(text, width.saturating_sub(2)) {
                    body.push(format!("  {line}"));
                }
            }
            // Absence is the finding, not a blank. Exact extraction
            // failing is itself why a check did not pass.
            None => body.push("  (the pane could not be read exactly)".into()),
        }
        body.push(String::new());
    }
    match &detail.loaded().body {
        Some(text) => body.extend(wrap(text, width)),
        None if detail.loaded().body_authorized => {
            body.push("  (message has no body)".into());
        }
        None => body.push("  (body not authorized for this reader)".into()),
    }
    if let Some(note) = &detail.loaded().thread_note {
        body.push(String::new());
        body.push(note.clone());
    }
    if !detail.loaded().thread.is_empty() {
        body.push(String::new());
        body.push(format!("thread, {} earlier", detail.loaded().thread.len()));
        for entry in &detail.loaded().thread {
            body.push(format!(
                "  {}  {}  {}",
                entry.message_id,
                entry.sender_label,
                entry.subject.as_deref().unwrap_or("")
            ));
            match &entry.body {
                Some(text) => {
                    for line in wrap(text, width.saturating_sub(4)) {
                        body.push(format!("    {line}"));
                    }
                }
                None => body.push("    (body not authorized, metadata only)".into()),
            }
        }
    }
    // The draft is the one thing on this surface the reader wrote, and a
    // multiline paste can make it the longest. One unwrapped line would
    // be cut by fit() with no sign that anything was lost, and an
    // embedded newline would break the line accounting the scroll offset
    // depends on. Wrap every line of it, blank lines included, so what is
    // on screen is what will be sent.
    let mut draft_end: Option<usize> = None;
    if !detail.draft().is_empty() || detail.is_composing() {
        body.push(String::new());
        body.push(if detail.is_composing() {
            "reply draft, still writing".into()
        } else {
            "reply draft".to_string()
        });
        // The caret marks where the next character goes. Without it a
        // composer with an empty draft, or one whose last line wrapped
        // exactly, looks like a surface that is not accepting input.
        let text = detail.draft().text();
        let shown = if detail.is_composing() {
            format!("{text}\u{2502}")
        } else {
            text.to_string()
        };
        for line in shown.split('\n') {
            if line.is_empty() {
                body.push("  ".into());
                continue;
            }
            for wrapped in wrap(line, width.saturating_sub(2)) {
                body.push(format!("  {wrapped}"));
            }
        }
        draft_end = Some(body.len().saturating_sub(1));
    }

    // Scrolled, not cut. The offset is clamped here so a body that
    // shrank under the reader cannot leave them looking past the end.
    let mut top = detail.scroll().min(body.len().saturating_sub(body_budget));
    // While composing, the view follows the caret. A reply typed under a
    // long thread was being written off the bottom of the frame, so the
    // reader could not see what they were sending.
    if let Some(end) = draft_end {
        if detail.is_composing() && body_budget > 0 && end >= top + body_budget {
            top = end + 1 - body_budget;
        }
    }
    for line in body.into_iter().skip(top).take(body_budget) {
        out.push(fit(&line, width));
    }
    while out.len() < height.saturating_sub(2) {
        out.push(fit("", width));
    }

    // The final rows say what can happen next. Connection state or a
    // notice gets its own row and never replaces those controls.
    // Composing owns the footer. The stage is still Open underneath, so
    // without this the numbered action list would sit there while the
    // reader types, advertising digits that are going into the draft as
    // characters. A footer that names keys doing something else is worse
    // than no footer.
    if detail.is_composing() {
        let reserved = 2 + usize::from(status.is_some());
        out.truncate(height.saturating_sub(reserved));
        while out.len() < height.saturating_sub(reserved) {
            out.push(fit("", width));
        }
        if let Some(status) = status {
            out.push(fit(status, width));
        }
        out.push(fit("ctrl-d send   enter newline   esc cancel", width));
        out.push(fit("esc back", width));
        out.truncate(height);
        while out.len() < height {
            out.push(fit("", width));
        }
        return out;
    }
    let footer = match detail.stage() {
        Stage::Confirming(action) => detail.confirmation(*action),
        Stage::Failed { why, .. } => format!("refused: {why}"),
        // No retry offered. These verbs are not idempotent, so a second
        // press could be refused, or could hide that the first landed.
        Stage::NotSent { action, why } => format!(
            "{} was not sent: {why}. nothing changed",
            action.map(|a| detail.action_word(a)).unwrap_or("the read")
        ),
        // Whatever is still safe is named and numbered here. A key that
        // works while the screen offers nothing is a key nobody knows
        // they have. The terminal verbs are already gone from allowed by
        // the time this renders, so what is listed is what may repeat.
        Stage::Uncertain { action, why } => {
            let what = action.map(|a| detail.action_word(a)).unwrap_or("the read");
            let safe: Vec<String> = detail
                .allowed()
                .iter()
                .enumerate()
                .map(|(i, a)| format!("{} {}", i + 1, detail.action_word(*a)))
                .collect();
            if safe.is_empty() {
                format!("{what} may have landed: {why}. esc, then reopen to see")
            } else {
                format!(
                    "{what} may have landed: {why}. esc to reopen, or {}",
                    safe.join("   ")
                )
            }
        }
        _ => {
            // Numbered, because the keyboard takes digits. An unnumbered
            // list asks the reader to count.
            let actions: Vec<String> = detail
                .allowed()
                .iter()
                .enumerate()
                .map(|(i, a)| format!("{} {}", i + 1, detail.action_word(*a)))
                .collect();
            if actions.is_empty() {
                "no actions available".to_string()
            } else {
                actions.join("   ")
            }
        }
    };
    // Wrapped, not cut. A confirmation carries a 40-character attempt id
    // and fit() truncates, so on any ordinary width the operator was
    // asked to say yes to a sentence that stopped before the thing it
    // was about. It takes as many rows as it needs; the body, which is
    // already scrollable, gives them up.
    let mut footer_lines = wrap(&footer, width);
    let status_rows = usize::from(status.is_some());
    footer_lines.truncate(height.saturating_sub(1 + status_rows));
    let reserved = footer_lines.len() + 1 + status_rows;
    out.truncate(height.saturating_sub(reserved));
    if let Some(status) = status {
        out.push(fit(status, width));
    }
    for line in footer_lines {
        out.push(fit(&line, width));
    }
    out.push(fit("esc back", width));
    out.truncate(height);
    while out.len() < height {
        out.push(fit("", width));
    }
    out
}

fn stage_word(stage: &Stage) -> &'static str {
    match stage {
        Stage::Opening => "opening",
        Stage::Open => "open",
        Stage::Confirming(_) => "confirm",
        Stage::Acting(_) => "working",
        Stage::Failed { .. } => "refused",
        Stage::NotSent { .. } => "not sent",
        Stage::Uncertain { .. } => "outcome unknown",
    }
}

fn tail(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    chars[chars.len().saturating_sub(6)..].iter().collect()
}
