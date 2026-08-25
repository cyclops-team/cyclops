//! The human work queue: one body-free list of what a person must act on.
//!
//! Backend-neutral, like [`crate::stream::Record`]. Nothing here opens a
//! socket, folds a journal, formats a colour, or performs an action. The
//! workspace Ratatui surface and the CLI are both thin readers of it.
//!
//! Three rules shape the whole module:
//!
//! - A row carries no message body. [`QueueRow`] has no field for one, so
//!   a body cannot reach a resting frame by mistake. It is a property of
//!   the type, not of the renderer's discipline.
//! - Selection is a stable id, never a row position. A snapshot replaced
//!   under the reader cannot retarget a keystroke, because there is no
//!   position to retarget.
//! - An action names one frozen id and the watermark it was read at. The
//!   daemon resolves that exact pair. The client never says "the current
//!   row" and never resolves anything itself.

use cyclops_proto::{
    MessageId, MessageRecipientRoute, NotificationAttemptId, NotificationAttentionCause,
    NotificationPreWriteCause, RecipientKey,
};

use crate::grid::display_width;

/// What one row IS, for its whole life.
///
/// A message in one recipient's mailbox, and nothing else. A broadcast
/// fans one message into a row per recipient, so the message id alone
/// names several rows and cannot address one; the pair can.
///
/// Deliberately not "message or alarm". An alarm is something that
/// happens TO a row, not a different kind of row: it appears, it is
/// cleared, it is requeued, the attempt behind it is replaced. Encoding
/// that in the identity made the identity change under the reader. The
/// cursor tracks a target, so a cleared alarm lost the cursor's row; an
/// open detail froze a target, so a successful clear left it reading
/// stale rather than cleared. The attempt rides alongside as the exact
/// thing an action names, and it is allowed to change.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QueueTarget {
    pub message_id: MessageId,
    pub recipient: RecipientKey,
}

impl QueueTarget {
    pub fn new(message_id: MessageId, recipient: RecipientKey) -> QueueTarget {
        QueueTarget {
            message_id,
            recipient,
        }
    }

    /// The id an operator reads. Display only: two rows of one broadcast
    /// show the same string and are still different targets.
    pub fn id(&self) -> String {
        self.message_id.to_string()
    }

    pub fn recipient(&self) -> Option<RecipientKey> {
        Some(self.recipient)
    }

    /// A total order for breaking a tie between two rows in one band at
    /// one sequence. Includes the recipient, so a broadcast's rows order
    /// deterministically instead of comparing equal.
    fn tiebreak(&self) -> String {
        format!("{}\u{1}{}", self.message_id, self.recipient)
    }
}

/// Where a message sits in its recipient's mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxWord {
    Pending,
    Claimed,
    DeliveredDirect,
    Superseded,
}

/// Where the one-shot wake attempt for that message stands.
///
/// Never rendered as delivered or read. `Notified` says a doorbell was
/// written, nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeWord {
    /// The daemon accepted an attempt into the recipient FIFO.
    Queued,
    /// The attempt is waiting for write readiness at the head of the FIFO.
    Gating,
    /// The durable write boundary was recorded before terminal mutation.
    Writing,
    /// The notification is visible in the terminal composer.
    Staged,
    /// The submit key was sent. This is not an agent acknowledgement.
    Submitted,
    /// The attempt stopped before any terminal bytes were written.
    BlockedBeforeWrite,
    Notified,
    /// The recipient claimed the message before this wake wrote anything.
    Withdrawn,
    /// An administrator retired the unwritten wake. The mailbox item remains.
    WithdrawnByOperator,
    NeedsAttention,
    /// Quota is still positively observed. Wait for a reset observation;
    /// the daemon will never resume this attempt automatically.
    QuotaHeld,
    /// Quota is no longer positively observed. An administrator may now
    /// explicitly requeue the whole message.
    QuotaResetObserved,
    /// A terminal action has durable recovery state but no final outcome.
    /// Separate fields say whether the key and task start were observed.
    ActionUncertain,
    Cleared,
    /// The operator proved the staged payload and submitted it.
    /// This says nothing about task completion.
    OperatorSubmitted,
    /// The operator proved and removed the staged payload.
    OperatorDiscarded,
    /// No attempt has been started for this row at all. Distinct from
    /// `Queued`: nothing has entered the recipient FIFO.
    NotStarted,
    /// A newer attempt replaced this one. Not an outcome, and not
    /// something an operator acts on.
    Superseded,
}

impl MailboxWord {
    /// Symbol and word. Both, always: colour is never the only encoding,
    /// and a symbol without a word is not readable on a screen reader.
    pub fn cell(self) -> &'static str {
        match self {
            MailboxWord::Pending => "* pending",
            MailboxWord::Claimed => "= claimed",
            MailboxWord::DeliveredDirect => "= direct",
            MailboxWord::Superseded => "- superseded",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            MailboxWord::Pending => "*pend",
            MailboxWord::Claimed => "=clmd",
            MailboxWord::DeliveredDirect => "=dir",
            MailboxWord::Superseded => "-sprs",
        }
    }
}

impl WakeWord {
    pub fn cell(self) -> &'static str {
        match self {
            WakeWord::Queued => ". queued",
            WakeWord::Gating => ". gating",
            WakeWord::Writing => "> writing",
            WakeWord::Staged => "> staged",
            WakeWord::Submitted => "^ submit sent",
            WakeWord::BlockedBeforeWrite => "! wake blocked",
            WakeWord::Notified => "> notified",
            WakeWord::Withdrawn => "= withdrawn",
            WakeWord::WithdrawnByOperator => "= wake withdrawn",
            WakeWord::NeedsAttention => "! needs attention",
            WakeWord::QuotaHeld => "! quota held",
            WakeWord::QuotaResetObserved => "! quota reset observed",
            WakeWord::ActionUncertain => "! action uncertain",
            WakeWord::Cleared => "x cleared",
            WakeWord::OperatorSubmitted => "^ submitted",
            WakeWord::OperatorDiscarded => "x discarded",
            WakeWord::NotStarted => "- not started",
            WakeWord::Superseded => "~ superseded",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            WakeWord::Queued => ".queue",
            WakeWord::Gating => ".gate",
            WakeWord::Writing => ">write",
            WakeWord::Staged => ">stage",
            WakeWord::Submitted => "^submit",
            WakeWord::BlockedBeforeWrite => "!block",
            WakeWord::Notified => ">notf",
            WakeWord::Withdrawn => "=wdrn",
            WakeWord::WithdrawnByOperator => "=wdraw",
            WakeWord::NeedsAttention => "!attn",
            WakeWord::QuotaHeld => "!quota",
            WakeWord::QuotaResetObserved => "!reset",
            WakeWord::ActionUncertain => "!uncertain",
            WakeWord::Cleared => "xclear",
            WakeWord::OperatorSubmitted => "^opsub",
            WakeWord::OperatorDiscarded => "xdiscd",
            WakeWord::NotStarted => "-nostart",
            WakeWord::Superseded => "~sprsd",
        }
    }
}

/// Which way the message travelled, from the reader's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Inbound,
    Outbound,
    /// Neither sent nor received by the reader. An administrator sees
    /// these; they belong in All and in neither mailbox scope.
    Observed,
}

/// One row. Metadata only.
///
/// There is deliberately no body field. The acceptance rule is that a
/// secret in a message body never appears in a resting frame, and the
/// cheapest way to guarantee that is to give the renderer nothing to
/// leak. A subject is metadata and stays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRow {
    pub target: QueueTarget,
    pub message_id: MessageId,
    /// Durable identity. Actions and selection key on this.
    pub recipient: RecipientKey,
    /// Current display chrome. A rename changes this and nothing else.
    pub recipient_label: String,
    pub subject: Option<String>,
    pub mailbox: MailboxWord,
    pub wake: WakeWord,
    pub cause: Option<NotificationAttentionCause>,
    pub pre_write_cause: Option<NotificationPreWriteCause>,
    /// Current live route. The immutable send-time label remains the fallback.
    pub current_route: Option<MessageRecipientRoute>,
    /// The daemon's one-based mailbox position.
    pub fifo_position: Option<u64>,
    /// Whether this row is work for the reader who asked.
    ///
    /// The daemon decides it, because only the daemon knows who asked.
    /// Reconstructing it here from direction and mailbox state would
    /// answer for the wrong person on any shared surface.
    pub needs_action: bool,
    /// The exact attempt an attention action names, when one exists.
    ///
    /// Separate from the identity on purpose: this is what changes as an
    /// alarm appears, clears, is requeued, or has its attempt replaced.
    pub attention: Option<NotificationAttemptId>,
    /// Durable pre-key terminal action intent for this attempt.
    ///
    /// Intent alone proves no key was accepted. A Discard intent may expose
    /// only exact-empty no-key reconciliation; Complete remains blocked.
    pub resolution_intent: Option<cyclops_proto::NotificationResolution>,
    /// Durable proof that the terminal accepted the intended action key.
    ///
    /// A value matching `resolution_intent` authorizes only no-key
    /// reconciliation of that same action. It never authorizes another key.
    pub resolution_action_accepted: Option<cyclops_proto::NotificationResolution>,
    /// Durable exact-payload hook or post-action claim evidence for Complete.
    ///
    /// Complete cannot reconcile without it. Discard does not start a turn
    /// and therefore does not require this field.
    pub resolution_consumption_observed:
        Option<cyclops_proto::NotificationResolutionConsumptionObservation>,
    /// Whether this reader may start a fresh resolution of THIS recipient's
    /// open alarm.
    ///
    /// The daemon's answer, never inferred. Direction, mailbox state and
    /// wake word are all things a client can see and none of them says
    /// who is allowed to act; only the daemon knows who asked. Matching
    /// durable-intent reconciliation is governed separately above.
    pub can_manage_attention: bool,
    /// Whether this reader may withdraw this exact unwritten wake.
    ///
    /// This is the daemon's answer. Visible state is not authorization.
    pub can_withdraw_notification: bool,
    /// The daemon's own FIFO position. The queue never invents an order.
    pub seq: u64,
    pub updated_at: u64,
    pub direction: Direction,
}

impl QueueRow {
    /// Does this row say a human is needed?
    pub fn needs_human(&self) -> bool {
        matches!(
            self.wake,
            WakeWord::NeedsAttention
                | WakeWord::BlockedBeforeWrite
                | WakeWord::QuotaHeld
                | WakeWord::QuotaResetObserved
                | WakeWord::ActionUncertain
        )
    }
}

/// What the reader is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Everything asking for this person's action. The default.
    Work,
    All,
    Inbox,
    Outbound,
}

impl Scope {
    pub fn word(self) -> &'static str {
        match self {
            Scope::Work => "work",
            Scope::All => "all",
            Scope::Inbox => "inbox",
            Scope::Outbound => "outbound",
        }
    }

    pub const ORDER: [Scope; 4] = [Scope::Work, Scope::All, Scope::Inbox, Scope::Outbound];

    fn admits(self, row: &QueueRow) -> bool {
        match self {
            Scope::All => true,
            // The daemon's answer, and only the daemon's answer. It is
            // the one party that knows who asked, and it already scopes
            // needs_action per recipient and gates the attention half on
            // admin. Adding needs_human() here overrode it: an observed
            // alarm on somebody else's mailbox entered a non-admin
            // reader's Work view and then offered them nothing they were
            // allowed to do. Wake state stays visible in All.
            Scope::Work => row.needs_action,
            Scope::Inbox => row.direction == Direction::Inbound,
            Scope::Outbound => row.direction == Direction::Outbound,
        }
    }
}

/// One authenticated read of the daemon's state, replaced whole.
///
/// The watermark is the workspace sequence these rows were read at. It
/// is client context for judging how old a read is. It is not a
/// precondition and no action carries it.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub watermark: u64,
    pub rows: Vec<QueueRow>,
}

/// One id and the snapshot it was read against.
///
/// Produced at the moment an operator confirms, never held between
/// actions. The client never sends a position; the id is what an action
/// names and what the daemon resolves.
///
/// The watermark is client context only. It is NOT sent and NOT checked:
/// a workspace sequence moves on unrelated traffic, so rejecting on it
/// would fail confirmations because somebody else sent a message. Safety
/// comes from the exact id plus the daemon's own preconditions at
/// mutation time. The watermark is kept for deciding whether a detail is
/// looking at a stale read, and for nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenTarget {
    pub target: QueueTarget,
    /// The attempt this detail was opened against, if it had one. Frozen
    /// with the row: an action names the attempt the operator read, not
    /// whichever one the row carries by the time they say yes.
    pub attempt: Option<NotificationAttemptId>,
    pub watermark: u64,
}

/// Counts for a header. Cheap, from the cached view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub visible: usize,
    pub attention: usize,
    pub pending: usize,
    pub total: usize,
}

/// The queue itself: rows, scope, and one selected id.
#[derive(Debug, Clone)]
pub struct HumanQueue {
    watermark: u64,
    /// Every row, ordered once on replacement: attention first, then the
    /// daemon's FIFO sequence within each band.
    rows: Vec<QueueRow>,
    scope: Scope,
    /// Indices into `rows` admitted by the current scope. Rebuilt only
    /// when the snapshot or the scope changes, so a keypress costs one
    /// index step rather than a filter over the whole snapshot.
    view: Vec<usize>,
    selected: Option<QueueTarget>,
    /// Counts for the header, computed when the view is rebuilt.
    ///
    /// Cached because a renderer asks for them every frame, and counting
    /// the whole snapshot in the draw loop makes frame cost follow the
    /// backlog instead of the window.
    counts: Counts,
    /// Position of `selected` inside `view`, kept in step with it.
    ///
    /// The id is the truth and the position is a cache. Without it every
    /// keypress scans the whole view to find where the cursor is, which
    /// is a linear cost per keystroke on a large snapshot.
    cursor: Option<usize>,
}

impl Default for HumanQueue {
    fn default() -> Self {
        HumanQueue::new()
    }
}

impl HumanQueue {
    pub fn new() -> HumanQueue {
        HumanQueue {
            watermark: 0,
            rows: Vec::new(),
            scope: Scope::Work,
            view: Vec::new(),
            counts: Counts::default(),
            selected: None,
            cursor: None,
        }
    }

    pub fn watermark(&self) -> u64 {
        self.watermark
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// Replace everything with one authenticated read.
    ///
    /// Selection is carried by id. If the selected row is gone, selection
    /// clears. Choosing its positional replacement would let an Enter that
    /// was typed against the old frame open a different message.
    pub fn replace(&mut self, snapshot: Snapshot) {
        let previous = self.selected.clone();
        let was_empty = self.view.is_empty();
        self.watermark = snapshot.watermark;
        // Kept exactly as the daemon handed them over. Order lives in
        // `view`, which sorts indices: reordering the rows themselves
        // moves a large struct per swap and costs more than the whole
        // rest of a snapshot replacement put together.
        self.rows = snapshot.rows;
        self.rebuild_view();
        match previous {
            Some(target) => self.place(self.view_position(&target)),
            None if was_empty && !self.view.is_empty() => self.place(Some(0)),
            None => self.place(None),
        }
    }

    pub fn set_scope(&mut self, scope: Scope) {
        if scope == self.scope {
            return;
        }
        let previous = self.selected.clone();
        self.scope = scope;
        self.rebuild_view();
        self.place(previous.and_then(|target| self.view_position(&target)));
    }

    /// Filter to the scope, then order: attention above inbox, and the
    /// daemon's own sequence inside each band. Ties break on the target
    /// id so two reads of one state produce one order.
    fn rebuild_view(&mut self) {
        let scope = self.scope;
        let rows = &self.rows;
        let mut view: Vec<usize> = (0..rows.len())
            .filter(|&i| scope.admits(&rows[i]))
            .collect();
        view.sort_by(|&a, &b| {
            let (a, b) = (&rows[a], &rows[b]);
            b.needs_human()
                .cmp(&a.needs_human())
                .then_with(|| a.seq.cmp(&b.seq))
                // Only reached when two rows share a band and a sequence,
                // so the allocation here is not on the common path.
                .then_with(|| a.target.tiebreak().cmp(&b.target.tiebreak()))
        });
        self.view = view;
        self.counts = Counts {
            visible: self.view.len(),
            attention: self.rows.iter().filter(|r| r.needs_human()).count(),
            pending: self
                .rows
                .iter()
                .filter(|r| r.mailbox == MailboxWord::Pending)
                .count(),
            total: self.rows.len(),
        };
    }

    /// Move the cursor to one position in the view, id and index together.
    fn place(&mut self, at: Option<usize>) {
        self.cursor = at.filter(|&i| i < self.view.len());
        self.selected = self.cursor.map(|i| self.rows[self.view[i]].target.clone());
        debug_assert_eq!(
            self.cursor.is_some(),
            self.selected.is_some(),
            "cursor and selection drifted apart"
        );
    }

    fn view_position(&self, target: &QueueTarget) -> Option<usize> {
        self.view
            .iter()
            .position(|&i| &self.rows[i].target == target)
    }

    fn selected_index(&self) -> Option<usize> {
        self.cursor
    }

    /// The rows the reader can see, in order.
    pub fn visible(&self) -> impl Iterator<Item = &QueueRow> {
        self.view.iter().map(move |&i| &self.rows[i])
    }

    pub fn len(&self) -> usize {
        self.view.len()
    }

    pub fn is_empty(&self) -> bool {
        self.view.is_empty()
    }

    pub fn selected(&self) -> Option<&QueueRow> {
        self.selected_index().map(|i| &self.rows[self.view[i]])
    }

    /// The row one target names, anywhere in the snapshot.
    ///
    /// Deliberately not scoped. Claiming a pending Work item takes it
    /// out of Work, and a detail that asked "is my row still visible"
    /// would call itself stale the moment its own claim succeeded, which
    /// is exactly when the reader still needs it.
    pub fn row_for(&self, target: &QueueTarget) -> Option<&QueueRow> {
        self.rows.iter().find(|row| &row.target == target)
    }

    /// Move the cursor onto one id. Refused if that id is not visible, so
    /// a caller cannot select something the reader cannot see.
    pub fn select(&mut self, target: &QueueTarget) -> bool {
        match self.view_position(target) {
            Some(at) => {
                self.place(Some(at));
                true
            }
            None => false,
        }
    }

    pub fn select_next(&mut self) {
        self.step(1);
    }

    pub fn select_previous(&mut self) {
        self.step(-1);
    }

    fn step(&mut self, delta: isize) {
        if self.view.is_empty() {
            self.place(None);
            return;
        }
        let last = self.view.len() as isize - 1;
        let next = match self.cursor {
            Some(current) => (current as isize + delta).clamp(0, last),
            None if delta < 0 => last,
            None => 0,
        };
        self.place(Some(next as usize));
    }

    /// The id and watermark an action carries.
    ///
    /// Taken at confirmation time and never held between actions. A
    /// caller that has one of these is holding the operator's decision,
    /// not the cursor's current whereabouts.
    pub fn freeze(&self) -> Option<FrozenTarget> {
        self.selected().map(|row| FrozenTarget {
            target: row.target.clone(),
            attempt: row.attention,
            watermark: self.watermark,
        })
    }

    /// Where the cursor sits in the visible order, for the viewport.
    /// Private: a position is not an identity, and nothing outside this
    /// module has any business acting on one.
    fn cursor_position(&self) -> Option<usize> {
        self.cursor
    }

    /// Header counts. Free: taken when the view was last rebuilt.
    pub fn counts(&self) -> Counts {
        self.counts
    }
}

/// First visible row, chosen so the cursor is always drawn.
///
/// Derived from the cursor rather than remembered, because the renderer
/// is pure: two calls with the same queue and the same size produce the
/// same frame. The cursor sits mid-window once the list is longer than
/// the window, and the window stops at the end of the list rather than
/// scrolling past it.
fn viewport_top(queue: &HumanQueue, body_rows: usize) -> usize {
    if body_rows == 0 || queue.len() <= body_rows {
        return 0;
    }
    let cursor = queue.cursor_position().unwrap_or(0);
    let last_top = queue.len() - body_rows;
    cursor.saturating_sub(body_rows / 2).min(last_top)
}

/// Cut or pad one cell to an exact display width.
pub(crate) fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = display_width(&ch.to_string());
        if used + w > width {
            // One column left over from a wide glyph is a space, not a
            // half-drawn character.
            while used < width {
                out.push(' ');
                used += 1;
            }
            return out;
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

/// The last six characters of an id: what fits in a sidebar.
fn id_suffix(target: &QueueTarget) -> String {
    let id = target.id();
    let chars: Vec<char> = id.chars().collect();
    let start = chars.len().saturating_sub(6);
    chars[start..].iter().collect()
}

/// The table's column widths, or None when a table cannot hold them.
///
/// Computed in one place so the decision to draw a table and the widths
/// the table then uses cannot disagree. They did: the threshold was a
/// guessed width, the columns were derived from the terminal, and
/// between 20 and 35 columns the row was built wider than the line and
/// the final trim cut the wake state off the end.
struct TableColumns {
    id_w: usize,
    who_w: usize,
    state_w: usize,
    wake_w: usize,
    subject_w: usize,
}

impl TableColumns {
    fn for_width(width: usize) -> Option<TableColumns> {
        let id_w = 12usize.min(width / 4);
        let who_w = 12usize.min(width / 5);
        let wide = width >= 96;
        let state_w = if wide { 18 } else { MAILBOX_SHORT_W };
        let wake_w = if wide { 18 } else { WAKE_SHORT_W };
        // Marker, flag, four cells, one space between each and one after.
        let fixed = 2 + id_w + 1 + who_w + 1 + state_w + 1 + wake_w + 1;
        // A table with nothing left for a subject is not a table; it is
        // the narrow band with two columns that do not fit.
        (fixed < width).then(|| TableColumns {
            id_w,
            who_w,
            state_w,
            wake_w,
            subject_w: width - fixed,
        })
    }
}

/// Widths of the two state cells at their short spelling.
const MAILBOX_SHORT_W: usize = 6;
const WAKE_SHORT_W: usize = 8;
/// Marker, flag, id, and the two state cells with one space each.
const NARROW_FIXED: usize = 2 + 1 + MAILBOX_SHORT_W + 1 + WAKE_SHORT_W;

/// Render the queue as exact-width lines.
///
/// Pure: takes a queue and a size, returns text. No colour, no cursor
/// escape, no daemon. A caller that wants colour paints the cells it
/// recognises; the words are already complete without it.
pub fn render(queue: &HumanQueue, width: usize, height: usize) -> Vec<String> {
    render_with_status(queue, width, height, None)
}

pub(crate) fn render_with_status(
    queue: &HumanQueue,
    width: usize,
    height: usize,
    status: Option<&str>,
) -> Vec<String> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let counts = queue.counts();
    let mut out: Vec<String> = Vec::with_capacity(height);

    // A sliver of a terminal still has to say the one thing that matters:
    // how much is waiting, and that there is somewhere to go.
    if height < 5 || width < 20 {
        out.push(fit(
            &format!("msg {}!{}", counts.pending, counts.attention),
            width,
        ));
        let body_rows = height.saturating_sub(2 + usize::from(status.is_some()));
        let selected = queue.selected().map(|r| &r.target);
        for row in queue
            .visible()
            .skip(viewport_top(queue, body_rows))
            .take(body_rows)
        {
            let marker = if selected == Some(&row.target) {
                ">"
            } else {
                " "
            };
            let flag = if row.needs_human() { "!" } else { " " };
            out.push(fit(
                &format!("{marker}{flag}{}", id_suffix(&row.target)),
                width,
            ));
        }
        if let Some(status) = status {
            if out.len() < height {
                out.push(fit(status, width));
            }
        }
        if out.len() < height {
            out.push(fit("enter  s scope", width));
        }
        while out.len() < height {
            out.push(fit("", width));
        }
        out.truncate(height);
        return out;
    }

    out.push(fit(
        &format!(
            "Messages  {}  {} shown  {} pending  {} attention",
            queue.scope().word(),
            counts.visible,
            counts.pending,
            counts.attention
        ),
        width,
    ));
    out.push(fit(&"-".repeat(width.min(160)), width));

    // Too narrow for a table, wide enough for the states. The two state
    // words are what an operator acts on, so the recipient and the
    // subject go first and the words stay whole. A table here would fit
    // its columns to the width and then cut the wake state off the end.
    let table = TableColumns::for_width(width);
    if table.is_none() {
        let body_rows = height.saturating_sub(3 + usize::from(status.is_some()));
        let id_w = width - NARROW_FIXED;
        for row in queue
            .visible()
            .skip(viewport_top(queue, body_rows))
            .take(body_rows)
        {
            let marker = if Some(&row.target) == queue.selected().map(|r| &r.target) {
                ">"
            } else {
                " "
            };
            let flag = if row.needs_human() { "!" } else { " " };
            out.push(fit(
                &format!(
                    "{marker}{flag}{} {} {}",
                    fit(&id_suffix(&row.target), id_w),
                    fit(row.mailbox.short(), MAILBOX_SHORT_W),
                    fit(row.wake.short(), WAKE_SHORT_W),
                ),
                width,
            ));
        }
        while out.len() < height.saturating_sub(1 + usize::from(status.is_some())) {
            out.push(fit("", width));
        }
        if let Some(status) = status {
            out.push(fit(status, width));
        }
        let footer = format!("{} shown  enter open  s scope", queue.counts().visible);
        out.push(fit(&footer, width));
        out.truncate(height);
        return out;
    }

    let TableColumns {
        id_w,
        who_w,
        state_w,
        wake_w,
        subject_w,
    } = table.expect("checked above");

    let body_rows = height.saturating_sub(3 + usize::from(status.is_some()));
    let selected = queue.selected().map(|r| r.target.clone());
    for row in queue
        .visible()
        .skip(viewport_top(queue, body_rows))
        .take(body_rows)
    {
        let marker = if Some(&row.target) == selected.as_ref() {
            ">"
        } else {
            " "
        };
        let flag = if row.needs_human() { "!" } else { " " };
        let (m, w) = if width >= 96 {
            (row.mailbox.cell(), row.wake.cell())
        } else {
            (row.mailbox.short(), row.wake.short())
        };
        let subject = row.subject.as_deref().unwrap_or("");
        out.push(fit(
            &format!(
                "{marker}{flag}{} {} {} {} {}",
                fit(&row.target.id(), id_w),
                fit(&row.recipient_label, who_w),
                fit(m, state_w),
                fit(w, wake_w),
                fit(subject, subject_w),
            ),
            width,
        ));
    }

    while out.len() < height.saturating_sub(1 + usize::from(status.is_some())) {
        out.push(fit("", width));
    }
    if let Some(status) = status {
        out.push(fit(status, width));
    }
    let footer = match queue.selected() {
        Some(row) => format!(
            "enter open   s scope   tab view   ? help   q quit   {}",
            row.target.id()
        ),
        None => "select a row   s scope   tab view   ? help   q quit".to_string(),
    };
    out.push(fit(&footer, width));
    out.truncate(height);
    out
}
