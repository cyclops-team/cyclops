//! Group-chat Messages experience: chat timeline, avatars, and bottom bounded composer.
//!
//! Pure data structures and renderers. Does not open sockets or issue IO directly.
//! Every timeline item displays strictly proven daemon facts without assuming wake or completion.

use std::collections::HashMap;

use cyclops_proto::{Kind, MessageId, NotificationAttentionCause, RecipientKey};

use crate::avatar::{Avatar, AvatarRegistry};
use crate::detail::{Detail, Draft, Stage, ThreadEntry};
use crate::grid::display_width;
use crate::queue::{fit, HumanQueue, MailboxWord, QueueRow, QueueTarget, WakeWord};

/// The mode and target context for the bottom bounded composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerMode {
    /// Replying to a specific durable message, bound to that MessageId and durable origin endpoint.
    Reply {
        message_id: MessageId,
        origin_endpoint: RecipientKey,
        origin_label: String,
        reply_subject: Option<String>,
    },
    /// Broadcasting an announcement expecting no reply, bound to resolved durable recipient endpoints.
    Announce {
        recipients: Vec<(RecipientKey, String)>,
    },
    /// Direct message to a specific resolved recipient endpoint.
    Direct {
        recipient_endpoint: RecipientKey,
        recipient_label: String,
    },
}

/// Exact routing derived from one composer mode at send time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerSendRoute {
    pub recipient_keys: Option<Vec<RecipientKey>>,
    pub fyi: bool,
    pub reply_to: Option<String>,
    pub subject: String,
}

impl ComposerMode {
    pub fn word(&self) -> &'static str {
        match self {
            ComposerMode::Reply { .. } => "Reply",
            ComposerMode::Announce { .. } => "Announce",
            ComposerMode::Direct { .. } => "Direct",
        }
    }

    /// Revalidate exact endpoints without converting them back to labels.
    /// Replies carry no selector because the daemon derives their immutable
    /// destination from the referenced message.
    pub fn revalidate_routes(
        &self,
        live_routes: &[cyclops_proto::StatusMailboxRoute],
    ) -> Result<ComposerSendRoute, String> {
        match self {
            ComposerMode::Reply { message_id, .. } => Ok(ComposerSendRoute {
                recipient_keys: None,
                fyi: false,
                reply_to: Some(message_id.to_string()),
                subject: format!("Re: {message_id}"),
            }),
            ComposerMode::Direct {
                recipient_endpoint, ..
            } => {
                live_routes
                    .iter()
                    .find(|r| &r.recipient == recipient_endpoint)
                    .ok_or_else(|| {
                        format!(
                            "recipient endpoint {recipient_endpoint} is no longer live in mailbox routes"
                        )
                    })?;
                Ok(ComposerSendRoute {
                    recipient_keys: Some(vec![*recipient_endpoint]),
                    fyi: false,
                    reply_to: None,
                    subject: "Direct Message".to_string(),
                })
            }
            ComposerMode::Announce { recipients } => {
                if recipients.is_empty() {
                    return Err("no recipients specified for announcement".to_string());
                }
                let mut recipient_keys = Vec::with_capacity(recipients.len());
                for (endpoint, _) in recipients {
                    live_routes
                        .iter()
                        .find(|r| &r.recipient == endpoint)
                        .ok_or_else(|| {
                            format!(
                                "announcement recipient {endpoint} is no longer live in mailbox routes"
                            )
                        })?;
                    recipient_keys.push(*endpoint);
                }
                Ok(ComposerSendRoute {
                    recipient_keys: Some(recipient_keys),
                    fyi: true,
                    reply_to: None,
                    subject: "Announcement".to_string(),
                })
            }
        }
    }
}

/// State of the bottom bounded composer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposerState {
    pub mode: Option<ComposerMode>,
    /// Exact authenticated mailbox sender bound by the consuming client.
    pub sender: Option<RecipientKey>,
    pub draft: Draft,
    pub stage: Option<Stage>,
    pub focused: bool,
}

impl ComposerState {
    pub fn new_reply(
        message_id: MessageId,
        origin_endpoint: RecipientKey,
        origin_label: String,
        reply_subject: Option<String>,
    ) -> Self {
        Self {
            mode: Some(ComposerMode::Reply {
                message_id,
                origin_endpoint,
                origin_label,
                reply_subject,
            }),
            sender: None,
            draft: Draft::default(),
            stage: None,
            focused: true,
        }
    }

    pub fn new_announce(recipients: Vec<(RecipientKey, String)>) -> Self {
        Self {
            mode: Some(ComposerMode::Announce { recipients }),
            sender: None,
            draft: Draft::default(),
            stage: None,
            focused: true,
        }
    }

    pub fn new_direct(recipient_endpoint: RecipientKey, recipient_label: String) -> Self {
        Self {
            mode: Some(ComposerMode::Direct {
                recipient_endpoint,
                recipient_label,
            }),
            sender: None,
            draft: Draft::default(),
            stage: None,
            focused: true,
        }
    }

    pub fn bind_sender(&mut self, sender: Option<RecipientKey>) {
        self.sender = sender;
    }

    pub fn push_char(&mut self, ch: char) -> bool {
        self.draft.push(ch)
    }

    pub fn backspace(&mut self) {
        self.draft.backspace();
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.draft.set(text);
    }

    pub fn text(&self) -> &str {
        self.draft.text()
    }

    pub fn is_empty(&self) -> bool {
        self.draft.is_empty()
    }

    pub fn key_for_send(&mut self, mint: impl FnOnce() -> String) -> String {
        self.draft.key_for_send(mint)
    }

    pub fn record_not_sent(&mut self, why: String) {
        self.stage = Some(Stage::NotSent { action: None, why });
    }

    pub fn record_uncertain(&mut self, why: String) {
        self.stage = Some(Stage::Uncertain { action: None, why });
    }

    pub fn record_failed(&mut self, why: String) {
        self.stage = Some(Stage::Failed { action: None, why });
    }

    pub fn clear_stage(&mut self) {
        self.stage = None;
    }

    /// Reconcile an uncertain send by clearing the stage after operator reconciliation.
    pub fn reconcile_stage(&mut self) {
        if matches!(self.stage, Some(Stage::Uncertain { .. })) {
            self.stage = None;
        }
    }
}

/// One verb the drawer's action strip offers.
///
/// The strip is the only place these verbs are named for a reader, and a
/// pointer must reach exactly the verb whose word it lands on, so the words
/// and their columns come from one table rather than from a literal that a
/// hit map then guesses at. Every verb here already has a keyboard route;
/// the strip is a second way to ask for the same action, never a second
/// implementation of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatAction {
    Reply,
    Announce,
    Open,
    /// Show or hide message bodies under their headings.
    Body,
    Scope,
    /// Clear the local presentation through its current durable sequence.
    /// The mailbox and journal remain unchanged.
    Clear,
    /// Flip the drawer between the messages of the session the operator is
    /// looking at and the messages of every watched session.
    Sessions,
    /// Only offered while a refresh has failed: the retry the status line
    /// tells the operator to press.
    Retry,
}

impl ChatAction {
    /// The word the strip prints for this verb, key hint included.
    pub fn label(self) -> &'static str {
        match self {
            ChatAction::Reply => "r reply",
            ChatAction::Announce => "a announce",
            ChatAction::Open => "enter open",
            ChatAction::Body => "b body",
            ChatAction::Scope => "s scope",
            ChatAction::Clear => "c clear",
            ChatAction::Sessions => "t sessions",
            ChatAction::Retry => "^R retry",
        }
    }

    /// The button as printed: the label with one cell of air on each side,
    /// so a click on the button's edge still lands on the verb and the
    /// painted fill has a shape rather than hugging the letters.
    pub fn button(self) -> String {
        format!(" {} ", self.label())
    }
}

/// The air between two buttons in the strip.
const ACTION_GAP: &str = " ";

/// The verbs the strip offers right now. `refresh_failed` adds the retry.
///
/// Every verb with a key on the Messages pane is here, [`ChatAction::Scope`]
/// excepted: that one is the header's chip. A verb bound to a key but
/// missing from the strip is a verb a pointer user cannot find.
pub fn chat_actions(refresh_failed: bool) -> Vec<ChatAction> {
    let mut actions = vec![
        ChatAction::Reply,
        ChatAction::Announce,
        ChatAction::Open,
        ChatAction::Body,
        ChatAction::Clear,
        ChatAction::Sessions,
    ];
    if refresh_failed {
        actions.insert(0, ChatAction::Retry);
    }
    actions
}

/// One action and its half-open column span in a rendered footer row.
pub type ChatActionSpan = (ChatAction, usize, usize);

/// One rendered footer row and every action span it contains.
pub type ChatActionStrip = (String, Vec<ChatActionSpan>);

/// The first action row as text, and where each verb sits in it.
///
/// Kept as a compatibility view for callers that inspect a comfortably
/// wide footer. Rendering uses [`chat_action_strips`] so narrow panes retain
/// every action on wrapped rows.
pub fn chat_action_strip(width: usize, refresh_failed: bool) -> ChatActionStrip {
    chat_action_strips(width, refresh_failed)
        .into_iter()
        .next()
        .unwrap_or_else(|| (String::new(), Vec::new()))
}

/// Every wrapped action row as text plus its clickable column spans.
pub fn chat_action_strips(width: usize, refresh_failed: bool) -> Vec<ChatActionStrip> {
    chat_action_lines(width, refresh_failed)
        .into_iter()
        .map(|line| {
            let mut spans = Vec::new();
            let mut col = 0usize;
            for span in &line.spans {
                let w = display_width(&span.text);
                if let ChatInk::Button(action) = span.ink {
                    spans.push((action, col, col + w));
                }
                col += w;
            }
            (line.text(), spans)
        })
        .collect()
}

/// The first centered action row.
///
/// Kept for callers whose layout already guarantees one row. New rendering
/// should use [`chat_action_lines`] so actions are never discarded.
pub fn chat_action_line(width: usize, refresh_failed: bool) -> ChatLine {
    chat_action_lines(width, refresh_failed)
        .into_iter()
        .next()
        .unwrap_or_else(|| ChatLine::new(ChatLineKind::Strip).fitted(width))
}

/// The footer as centered wrapped rows. Every action is retained.
///
/// Rows are greedily packed in action order. If a pane becomes narrower than
/// one complete button, that button still owns its own fitted row and remains
/// clickable instead of silently disappearing.
pub fn chat_action_lines(width: usize, refresh_failed: bool) -> Vec<ChatLine> {
    if width == 0 {
        return Vec::new();
    }

    let mut rows: Vec<Vec<(ChatAction, String)>> = Vec::new();
    let mut row: Vec<(ChatAction, String)> = Vec::new();
    let mut used = 0usize;
    for action in chat_actions(refresh_failed) {
        let button = action.button();
        let gap = usize::from(!row.is_empty()) * display_width(ACTION_GAP);
        let button_width = display_width(&button);
        if !row.is_empty() && used + gap + button_width > width {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        let gap = usize::from(!row.is_empty()) * display_width(ACTION_GAP);
        used += gap + button_width;
        row.push((action, button));
    }
    if !row.is_empty() {
        rows.push(row);
    }

    rows.into_iter()
        .map(|buttons| {
            let used = buttons
                .iter()
                .map(|(_, button)| display_width(button))
                .sum::<usize>()
                + buttons.len().saturating_sub(1) * display_width(ACTION_GAP);
            let mut line = ChatLine::new(ChatLineKind::Strip);
            line.push(" ".repeat(width.saturating_sub(used) / 2), ChatInk::Text);
            for (index, (action, button)) in buttons.into_iter().enumerate() {
                if index > 0 {
                    line.push(ACTION_GAP, ChatInk::Text);
                }
                line.push(button, ChatInk::Button(action));
            }
            line.fitted(width)
        })
        .collect()
}

/// How one run of text in a drawer line is inked.
///
/// The renderer names what a run *is*; the surface that paints it owns the
/// palette. `cyclops-ui` has no colors, so a name here is a fact about the
/// text (this is an agent's name, this needs a person) and never a hue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatInk {
    /// Ordinary text.
    Text,
    /// Secondary detail: times, ids, arrows, rules.
    Dim,
    /// The selection mark, the panel's own word, the `@all` address.
    Accent,
    /// An agent's name, keyed by its display label. The workspace paints
    /// it in the same stable role color the agent's pane border and
    /// sidebar row already carry, so a name means one thing everywhere.
    Role(String),
    /// The avatar chip for a label: the role color as ground, the panel
    /// ink on top. Same key as [`ChatInk::Role`] so chip and name match.
    Avatar(String),
    /// A wake or mailbox fact that needs a person.
    Attention,
    /// A wake or mailbox fact that ended well: claimed, delivered, notified.
    Healthy,
    /// One footer button. The surface lights it under the pointer.
    Button(ChatAction),
}

/// One inked run of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatSpan {
    pub text: String,
    pub ink: ChatInk,
}

/// What a drawer line is, so a surface can find the strip or the header
/// without matching on the words the renderer happened to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatLineKind {
    Header,
    /// A line of one message: its heading, body, or thread history.
    Message,
    /// One recipient's proven mailbox and wake fact.
    Status,
    /// The bar above the footer.
    Rule,
    /// The one-line status row above the footer, when there is one.
    Notice,
    Composer,
    /// The footer button bar.
    Strip,
    Blank,
}

/// The queue rows one timeline line speaks for.
///
/// A message heading carries every recipient of its message; a
/// recipient's own status line carries exactly that one. A surface maps a
/// click on the line to a selection through it, and the frame paints the
/// cursor on every line whose owner holds the selected target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineOwner {
    pub message_id: MessageId,
    pub targets: Vec<QueueTarget>,
}

/// One exact-width line of the drawer: inked runs that concatenate to the
/// plain row [`render_chat`] returns.
///
/// An owned line's first span starts with one blank cell: the cursor
/// cell, painted `>` by [`ChatLine::mark_cursor`] when its owner is
/// selected. Keeping the mark out of the build is what lets a built
/// timeline be reused while the cursor moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLine {
    pub kind: ChatLineKind,
    pub spans: Vec<ChatSpan>,
    pub owner: Option<LineOwner>,
}

impl ChatLine {
    pub fn new(kind: ChatLineKind) -> Self {
        Self {
            kind,
            spans: Vec::new(),
            owner: None,
        }
    }

    /// Name the rows this line speaks for.
    pub fn owned(mut self, owner: LineOwner) -> Self {
        self.owner = Some(owner);
        self
    }

    /// Paint the cursor cell. Only an owned line has one; the first cell
    /// of its first span is blank until the line is selected.
    pub fn mark_cursor(&mut self) {
        if self.owner.is_none() {
            return;
        }
        if let Some(first) = self.spans.first_mut() {
            if first.text.starts_with(' ') {
                first.text.replace_range(..1, ">");
            }
        }
    }

    /// The message this line speaks for, if any.
    pub fn message_id(&self) -> Option<&MessageId> {
        self.owner.as_ref().map(|owner| &owner.message_id)
    }

    /// Append one run. An empty run is dropped rather than kept as a
    /// zero-width span a painter would have to step over.
    pub fn push(&mut self, text: impl Into<String>, ink: ChatInk) -> &mut Self {
        let text = text.into();
        if !text.is_empty() {
            self.spans.push(ChatSpan { text, ink });
        }
        self
    }

    /// The row as plain text, every run concatenated in order.
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    /// Display columns the row occupies.
    pub fn width(&self) -> usize {
        self.spans
            .iter()
            .map(|span| display_width(&span.text))
            .sum()
    }

    /// Cut or pad to exactly `width` cells, the way [`fit`] does for plain
    /// text, keeping the ink of every run that survives the cut.
    pub fn fitted(mut self, width: usize) -> Self {
        let mut kept = Vec::with_capacity(self.spans.len() + 1);
        let mut used = 0usize;
        for span in self.spans.drain(..) {
            if used >= width {
                break;
            }
            let room = width - used;
            let w = display_width(&span.text);
            if w <= room {
                used += w;
                kept.push(span);
            } else {
                kept.push(ChatSpan {
                    text: fit(&span.text, room),
                    ink: span.ink,
                });
                used = width;
                break;
            }
        }
        if used < width {
            kept.push(ChatSpan {
                text: " ".repeat(width - used),
                ink: ChatInk::Text,
            });
        }
        self.spans = kept;
        self
    }
}

/// A line of plain text, fitted.
fn plain(kind: ChatLineKind, text: &str, width: usize) -> ChatLine {
    let mut line = ChatLine::new(kind);
    line.push(text, ChatInk::Text);
    line.fitted(width)
}

/// Break text into rows no wider than `width`, on spaces where there are
/// spaces and inside a word only when the word alone is wider than the
/// row. Existing line breaks are kept; a blank line stays a blank line.
///
/// A drawer that cut every body at its right edge lost the end of every
/// sentence longer than the panel, which on a narrow drawer was most of
/// them. Wrapping costs rows; losing the words cost the message.
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for raw in text.split('\n') {
        let mut row = String::new();
        let mut row_w = 0usize;
        for word in raw.split_whitespace() {
            let word_w = display_width(word);
            if row_w > 0 && row_w + 1 + word_w <= width {
                row.push(' ');
                row.push_str(word);
                row_w += 1 + word_w;
                continue;
            }
            if row_w > 0 {
                rows.push(std::mem::take(&mut row));
                row_w = 0;
            }
            if word_w <= width {
                row.push_str(word);
                row_w = word_w;
                continue;
            }
            // A single word wider than the row: cut it at cell boundaries.
            for ch in word.chars() {
                let w = display_width(&ch.to_string());
                if row_w + w > width && row_w > 0 {
                    rows.push(std::mem::take(&mut row));
                    row_w = 0;
                }
                row.push(ch);
                row_w += w;
            }
        }
        rows.push(row);
    }
    rows
}

/// Computes the exact proven delivery truth label from mailbox and wake states.
pub fn proven_status_label(mailbox: MailboxWord, wake: WakeWord) -> &'static str {
    match (mailbox, wake) {
        (_, WakeWord::Withdrawn) => "Withdrawn",
        (_, WakeWord::WithdrawnByOperator) => "Wake withdrawn",
        (MailboxWord::Claimed, _) => "Claimed",
        (_, WakeWord::NeedsAttention) => "Attention",
        (_, WakeWord::QuotaHeld) => "Quota held",
        (_, WakeWord::QuotaResetObserved) => "Quota reset",
        (_, WakeWord::BlockedBeforeWrite) => "Blocked before write",
        (_, WakeWord::ResolutionIncomplete) => "Resolution open",
        (_, WakeWord::Superseded) => "Superseded",
        (_, WakeWord::Queued) => "Wake queued",
        (_, WakeWord::Gating) => "Wake gating",
        (_, WakeWord::Writing) => "Wake writing",
        (_, WakeWord::Staged) => "Wake staged",
        (_, WakeWord::Submitted) => "Wake submit sent",
        (_, WakeWord::SubmittedUnverified) => "Wake submit sent (unverified)",
        (_, WakeWord::Notified) => "Wake notified",
        (_, WakeWord::ResolvedSubmitted) => "Wake submitted",
        (_, WakeWord::ResolvedDiscarded) => "Wake discarded",
        (_, WakeWord::Cleared) => "Cleared",
        (MailboxWord::Pending, WakeWord::NotStarted) => "Accepted (wake not started)",
        (MailboxWord::DeliveredDirect, _) => "Delivered direct",
        (MailboxWord::Superseded, _) => "Superseded",
    }
}

/// Short code for narrow display.
pub fn proven_status_short(mailbox: MailboxWord, wake: WakeWord) -> &'static str {
    match (mailbox, wake) {
        (_, WakeWord::Withdrawn | WakeWord::WithdrawnByOperator) => "=wdrn",
        (MailboxWord::Claimed, _) => "=claim",
        (_, WakeWord::NeedsAttention) => "!attn",
        (_, WakeWord::QuotaHeld) => "!quota",
        (_, WakeWord::QuotaResetObserved) => "!reset",
        (_, WakeWord::BlockedBeforeWrite) => "!block",
        (_, WakeWord::ResolutionIncomplete) => "!incomp",
        (_, WakeWord::Superseded) => "-sprsd",
        (_, WakeWord::Cleared | WakeWord::ResolvedDiscarded) => "xclear",
        (_, WakeWord::Queued | WakeWord::Gating | WakeWord::Writing | WakeWord::Staged) => {
            ".wake-pend"
        }
        (
            _,
            WakeWord::Submitted
            | WakeWord::SubmittedUnverified
            | WakeWord::Notified
            | WakeWord::ResolvedSubmitted,
        ) => "^wake-sent",
        (MailboxWord::Pending, WakeWord::NotStarted) => "*acc-nostart",
        (MailboxWord::DeliveredDirect, _) => "=dir",
        (MailboxWord::Superseded, _) => "-sprsd",
    }
}

/// Human-readable words for attention causes.
pub fn attention_cause_label(cause: NotificationAttentionCause) -> &'static str {
    match cause {
        NotificationAttentionCause::PasteFailed => "paste failed",
        NotificationAttentionCause::VerifyFailed => "verify failed",
        NotificationAttentionCause::PaneReboundAfterPaste => "pane rebound after paste",
        NotificationAttentionCause::SubmitFailed => "submit failed",
        NotificationAttentionCause::ReceiptOccupantChanged => "receipt occupant changed",
        NotificationAttentionCause::AckTimeout => "ack timeout",
        NotificationAttentionCause::DaemonRestart => "daemon restart",
        NotificationAttentionCause::TransportOutcomeUnknown => "transport outcome unknown",
    }
}

/// A recipient delivery fact entry on an aggregated message bubble.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientEntry {
    pub recipient: RecipientKey,
    pub label: String,
    pub avatar: Avatar,
    pub mailbox: MailboxWord,
    pub wake: WakeWord,
    pub cause: Option<String>,
    pub fifo_position: Option<u64>,
    /// When this recipient-specific mailbox/wake projection last changed.
    /// Attention decay follows the hold, not the older message envelope.
    pub updated_at: u64,
    pub is_attention: bool,
    pub target: QueueTarget,
}

/// A structured timeline message entry for rendering, aggregated by MessageId.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub message_id: MessageId,
    pub is_broadcast: bool,
    pub sender: RecipientKey,
    pub sender_label: String,
    pub sender_avatar: Avatar,
    pub recipients: Vec<RecipientEntry>,
    pub subject: Option<String>,
    pub reply_to: Option<MessageId>,
    pub thread_root: MessageId,
    pub thread_message_count: u64,
    pub ts: u64,
    /// The body the daemon handed to the open detail, or to its thread.
    pub authorized_body: Option<String>,
    pub thread_history: Vec<ThreadEntry>,
    /// This message is the one the open detail froze.
    pub detail_open: bool,
}

impl TimelineItem {
    /// The rows a heading speaks for: every recipient.
    pub fn owner(&self) -> LineOwner {
        LineOwner {
            message_id: self.message_id.clone(),
            targets: self.recipients.iter().map(|r| r.target.clone()).collect(),
        }
    }

    /// The one row a recipient's status line speaks for.
    pub fn owner_of(&self, recipient: &RecipientEntry) -> LineOwner {
        LineOwner {
            message_id: self.message_id.clone(),
            targets: vec![recipient.target.clone()],
        }
    }

    /// Aggregate QueueRows by MessageId into timeline entries.
    pub fn aggregate_from_rows(
        rows: &[&QueueRow],
        avatar_registry: &AvatarRegistry,
        live_routes: Option<&[cyclops_proto::StatusMailboxRoute]>,
        pane_manifests: Option<&HashMap<String, String>>,
        detail: Option<&Detail>,
    ) -> Vec<Self> {
        let mut grouped: Vec<TimelineItem> = Vec::new();
        let mut index_by_id: HashMap<MessageId, usize> = HashMap::new();

        for row in rows {
            let recip_avatar = avatar_registry.resolve_route_endpoint(
                &row.recipient,
                &row.recipient_label,
                live_routes,
                pane_manifests,
            );

            let cause_str = row
                .cause
                .as_ref()
                .map(|c| attention_cause_label(*c).to_string())
                // The named block is more exact than the enum cause.
                .or_else(|| row.pre_write_block.as_deref().map(crate::grid::cause_words))
                .or_else(|| row.pre_write_cause.as_ref().map(|c| c.label().to_string()));

            let recip_entry = RecipientEntry {
                recipient: row.recipient,
                label: row.recipient_label.clone(),
                avatar: recip_avatar,
                mailbox: row.mailbox,
                wake: row.wake,
                cause: cause_str,
                fifo_position: row.fifo_position,
                updated_at: row.updated_at,
                is_attention: row.needs_human(),
                target: row.target.clone(),
            };

            if let Some(&idx) = index_by_id.get(&row.message_id) {
                let item = &mut grouped[idx];
                item.recipients.push(recip_entry);
                item.is_broadcast = item.recipients.len() > 1 || row.kind == Kind::Fyi;
            } else {
                let sender_avatar = avatar_registry.resolve_route_endpoint(
                    &row.sender,
                    &row.sender_label,
                    live_routes,
                    pane_manifests,
                );

                let is_broadcast = row.recipient_count > 1 || row.kind == Kind::Fyi;

                let detail_open =
                    detail.is_some_and(|d| d.target().target.message_id == row.message_id);
                let (authorized_body, thread_history) = if let Some(d) = detail {
                    let loaded = d.loaded();
                    if detail_open {
                        let row_msg_ids: std::collections::HashSet<&str> =
                            rows.iter().map(|r| r.message_id.as_str()).collect();
                        let filtered_history: Vec<ThreadEntry> = loaded
                            .thread
                            .iter()
                            .filter(|e| !row_msg_ids.contains(e.message_id.as_str()))
                            .cloned()
                            .collect();
                        (loaded.body.clone(), filtered_history)
                    } else if let Some(entry) = loaded
                        .thread
                        .iter()
                        .find(|e| e.message_id == row.message_id.as_str())
                    {
                        (entry.body.clone(), Vec::new())
                    } else {
                        (None, Vec::new())
                    }
                } else {
                    (None, Vec::new())
                };

                let item = TimelineItem {
                    message_id: row.message_id.clone(),
                    is_broadcast,
                    sender: row.sender,
                    sender_label: row.sender_label.clone(),
                    sender_avatar,
                    recipients: vec![recip_entry],
                    subject: row.subject.clone(),
                    reply_to: row.reply_to.clone(),
                    thread_root: row.thread_root.clone(),
                    thread_message_count: row.thread_message_count,
                    ts: row.ts,
                    authorized_body,
                    thread_history,
                    detail_open,
                };
                index_by_id.insert(row.message_id.clone(), grouped.len());
                grouped.push(item);
            }
        }

        grouped
    }
}

/// Formats a relative timestamp from Unix ms and current time in ms.
pub fn format_time(ts_ms: u64, now_ms: Option<u64>) -> String {
    if ts_ms == 0 {
        return "-".to_string();
    }
    if let Some(now) = now_ms {
        if now >= ts_ms {
            let diff_ms = now - ts_ms;
            if diff_ms < 1_000 {
                return "just now".to_string();
            } else if diff_ms < 60_000 {
                return format!("{}s ago", diff_ms / 1_000);
            } else if diff_ms < 3_600_000 {
                return format!("{}m ago", diff_ms / 60_000);
            } else if diff_ms < 86_400_000 {
                return format!("{}h ago", diff_ms / 3_600_000);
            } else {
                return format!("{}d ago", diff_ms / 86_400_000);
            }
        }
    }
    format!("{ts_ms}ms")
}

/// How long an unresolved hold keeps its full attention voice.
///
/// Past this it is still shown and still exactly as accurate; it just
/// stops competing with what happened seconds ago. Nothing is hidden and
/// no word changes, only the ink: a four-hour-old attempt that shouts as
/// loudly as a fresh one teaches an operator to ignore the colour, which
/// costs the fresh one its meaning.
const HELD_LOUD_MS: u64 = 15 * 60 * 1000;

/// Attention ink while a hold is fresh, dim once it is old news.
///
/// With no clock the hold keeps its full voice: a renderer that cannot
/// tell how old something is must not quietly decide it is stale.
fn held_ink(ts: u64, now_ms: Option<u64>) -> ChatInk {
    match now_ms {
        Some(now) if now.saturating_sub(ts) > HELD_LOUD_MS => ChatInk::Dim,
        _ => ChatInk::Attention,
    }
}

/// The hold phrase for one recipient: the exact cause, plus its queue
/// position when that is what an operator needs to act. Byte-identical
/// to what the daemon proved; this only chooses where it sits.
fn held_words(r: &RecipientEntry, status_label: &str, behind: usize) -> String {
    let cause_desc = r.cause.as_deref().unwrap_or(status_label);
    if r.mailbox == MailboxWord::Pending && r.fifo_position == Some(1) {
        if behind > 0 {
            format!("head · held: {cause_desc} · {behind} behind")
        } else {
            format!("head · held: {cause_desc}")
        }
    } else {
        match r.fifo_position {
            Some(pos) => format!("held: {cause_desc} · pos {pos}"),
            None => format!("held: {cause_desc}"),
        }
    }
}

/// First timeline line, centered on the selected recipient when possible.
fn timeline_viewport_top(
    total_lines: usize,
    visible_lines: usize,
    selected_line: Option<usize>,
) -> usize {
    if visible_lines == 0 || total_lines <= visible_lines {
        return 0;
    }
    let last_top = total_lines - visible_lines;
    selected_line
        .map(|line| line.saturating_sub(visible_lines / 2).min(last_top))
        .unwrap_or(last_top)
}

/// What one operator read of a message body came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyState {
    /// Asked, not yet answered.
    Loading,
    /// The daemon authorized the read. `None` is a message with no body,
    /// which is not the same as a body the reader may not see.
    Loaded(Option<String>),
    /// The daemon refused the read, or does not offer it.
    Unavailable,
}

/// Bodies read through `msg.read`, one per message id.
///
/// A body is immutable once accepted, so an answer is kept for the life
/// of the surface. The revision counts changes, so a cached timeline can
/// tell that it was built before an answer landed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageBodies {
    by_id: HashMap<MessageId, BodyState>,
    revision: u64,
}

impl MessageBodies {
    pub fn get(&self, message_id: &MessageId) -> Option<&BodyState> {
        self.by_id.get(message_id)
    }

    pub fn contains(&self, message_id: &MessageId) -> bool {
        self.by_id.contains_key(message_id)
    }

    pub fn set(&mut self, message_id: MessageId, state: BodyState) {
        self.by_id.insert(message_id, state);
        self.revision = self.revision.wrapping_add(1);
    }

    /// Forget an unanswered read so it can be asked again.
    pub fn forget_loading(&mut self, message_id: &MessageId) {
        if self.by_id.get(message_id) == Some(&BodyState::Loading) {
            self.by_id.remove(message_id);
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// Read-only context used to paint one Messages frame.
#[derive(Clone, Copy)]
pub struct ChatRenderContext<'a> {
    pub detail: Option<&'a Detail>,
    pub composer: Option<&'a ComposerState>,
    pub avatar_registry: &'a AvatarRegistry,
    pub live_routes: Option<&'a [cyclops_proto::StatusMailboxRoute]>,
    pub pane_manifests: Option<&'a HashMap<String, String>>,
    pub status: Option<&'a str>,
    /// A failed one-shot snapshot can be retried through Ctrl+R. Passive
    /// subscription reconnects stay status-only because they have no
    /// operator-triggered action.
    pub retry_available: bool,
    pub now_ms: Option<u64>,
    pub view_journal: bool,
    /// Full rows: bodies, thread and every recipient's fact under each
    /// heading. Off is the compact default, one heading per message.
    pub show_bodies: bool,
    /// Bodies the operator read through `msg.read`, shown only while
    /// `show_bodies` is on.
    pub bodies: Option<&'a MessageBodies>,
}

impl<'a> ChatRenderContext<'a> {
    pub fn new(avatar_registry: &'a AvatarRegistry) -> Self {
        Self {
            detail: None,
            composer: None,
            avatar_registry,
            live_routes: None,
            pane_manifests: None,
            status: None,
            retry_available: false,
            now_ms: None,
            view_journal: false,
            show_bodies: false,
            bodies: None,
        }
    }

    pub fn with_show_bodies(mut self, show_bodies: bool) -> Self {
        self.show_bodies = show_bodies;
        self
    }

    pub fn with_bodies(mut self, bodies: &'a MessageBodies) -> Self {
        self.bodies = Some(bodies);
        self
    }

    pub fn with_detail(mut self, detail: &'a Detail) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn with_composer(mut self, composer: &'a ComposerState) -> Self {
        self.composer = Some(composer);
        self
    }

    pub fn with_status(mut self, status: &'a str) -> Self {
        self.status = Some(status);
        self
    }

    pub fn with_retry_available(mut self) -> Self {
        self.retry_available = true;
        self
    }

    pub fn with_view_journal(mut self, view_journal: bool) -> Self {
        self.view_journal = view_journal;
        self
    }

    pub fn at(mut self, now_ms: u64) -> Self {
        self.now_ms = Some(now_ms);
        self
    }
}

/// The avatar as a chip: the badge with one cell of air each side, so the
/// painted ground has a shape a reader recognises as an avatar.
fn chip(avatar: &Avatar) -> String {
    format!(" {} ", avatar.badge())
}

/// The glyph and ink for one recipient's proven state.
fn status_ink(entry: &RecipientEntry) -> (&'static str, ChatInk) {
    if entry.is_attention {
        return ("!", ChatInk::Attention);
    }
    let healthy = matches!(
        entry.mailbox,
        MailboxWord::Claimed | MailboxWord::DeliveredDirect
    ) || matches!(
        entry.wake,
        WakeWord::Submitted | WakeWord::Notified | WakeWord::ResolvedSubmitted
    );
    if healthy {
        ("✓", ChatInk::Healthy)
    } else {
        ("·", ChatInk::Dim)
    }
}

/// A status hint that names a failure is inked for attention; the rest
/// (reconnecting, a passing notice) is detail.
fn hint_ink(hint: &str) -> ChatInk {
    if hint.contains("failed") || hint.contains("refused") {
        ChatInk::Attention
    } else {
        ChatInk::Dim
    }
}

/// Render the group-chat timeline and bottom bounded composer into plain
/// exact-width lines: [`render_chat_lines`] with the ink dropped, for
/// readers that only want the words.
pub fn render_chat(
    queue: &HumanQueue,
    context: ChatRenderContext<'_>,
    width: usize,
    height: usize,
) -> Vec<String> {
    render_chat_lines(queue, context, width, height)
        .iter()
        .map(ChatLine::text)
        .collect()
}

/// The timeline built once for one queue state, width and body choice,
/// ready to be sliced into frames. Header and footer are not here: they
/// are cheap and depend on the composer, which changes with every key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTimeline {
    pub lines: Vec<ChatLine>,
}

impl ChatTimeline {
    /// The line the viewport centers on: the first line that speaks for
    /// the selected target.
    pub fn anchor_line(&self, selected: Option<&QueueTarget>) -> Option<usize> {
        let selected = selected?;
        self.lines.iter().position(|line| {
            line.owner
                .as_ref()
                .is_some_and(|owner| owner.targets.contains(selected))
        })
    }
}

/// Where a frame's timeline window sat, for a caller that scrolls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChatViewport {
    /// First timeline line shown.
    pub top: usize,
    /// Timeline rows the frame had room for.
    pub visible: usize,
    /// Timeline lines in total.
    pub total: usize,
}

/// One painted frame: exact-width inked rows and the window they show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatFrame {
    pub lines: Vec<ChatLine>,
    pub viewport: ChatViewport,
}

/// Render the group-chat timeline and bottom bounded composer into
/// exact-width inked lines: [`build_chat_timeline`] then
/// [`render_chat_frame`], for callers that do not keep the timeline.
pub fn render_chat_lines(
    queue: &HumanQueue,
    context: ChatRenderContext<'_>,
    width: usize,
    height: usize,
) -> Vec<ChatLine> {
    let timeline = build_chat_timeline(queue, context, width);
    render_chat_frame(&timeline, queue, context, width, height, None).lines
}

/// Build every timeline line for the queue's visible rows at `width`.
///
/// Selection is not an input: the cursor is painted by
/// [`render_chat_frame`], so moving it costs a slice and not a rebuild.
/// Everything else the lines depend on (rows, scope, session filter,
/// avatars, the open detail, read bodies, the clock the relative times
/// use) is, and a caller that caches the result must rebuild when any of
/// those change.
pub fn build_chat_timeline(
    queue: &HumanQueue,
    context: ChatRenderContext<'_>,
    width: usize,
) -> ChatTimeline {
    let ChatRenderContext {
        detail,
        avatar_registry,
        live_routes,
        pane_manifests,
        now_ms,
        view_journal,
        show_bodies,
        bodies,
        ..
    } = context;
    if width == 0 {
        return ChatTimeline { lines: Vec::new() };
    }
    let visible_rows: Vec<&QueueRow> = queue.visible().collect();
    let items = TimelineItem::aggregate_from_rows(
        &visible_rows,
        avatar_registry,
        live_routes,
        pane_manifests,
        detail,
    );

    let mut timeline: Vec<ChatLine> = Vec::new();
    let hidden = queue.hidden_pending();
    if items.is_empty() && hidden > 0 {
        let mut notice = ChatLine::new(ChatLineKind::Status);
        notice.push(
            format!("  ! {} pending hidden by scope · press s for all", hidden),
            ChatInk::Attention,
        );
        timeline.push(plain(ChatLineKind::Blank, "", width));
        timeline.push(notice.fitted(width));
    }
    let pending_behind = |entry: &RecipientEntry| {
        visible_rows
            .iter()
            .filter(|row| row.recipient == entry.recipient && row.mailbox == MailboxWord::Pending)
            .count()
            .saturating_sub(1)
    };

    if width < 24 {
        // Ultra-narrow: one line per message with initials (never icon
        // only), one line per recipient, and the hold lines.
        for item in &items {
            let attn_mark = if item.recipients.iter().any(|r| r.is_attention) {
                "!"
            } else {
                " "
            };
            let s_initials = &item.sender_avatar.initials;
            let id = item.message_id.as_str();
            let short = &id[id.len().saturating_sub(6)..];
            let status_short = item
                .recipients
                .first()
                .map(|r| proven_status_short(r.mailbox, r.wake))
                .unwrap_or("*pend");
            let mut line1 = ChatLine::new(ChatLineKind::Message);
            line1.push(" ", ChatInk::Accent);
            line1.push(
                format!("{attn_mark}[{s_initials}]{short} {status_short}"),
                ChatInk::Text,
            );
            timeline.push(line1.owned(item.owner()).fitted(width));
            for r in &item.recipients {
                let mut line = ChatLine::new(ChatLineKind::Message);
                line.push(" ", ChatInk::Accent);
                line.push(format!("-> {}", r.label), ChatInk::Text);
                timeline.push(line.owned(item.owner_of(r)).fitted(width));
                if r.is_attention || r.cause.is_some() {
                    let mut line = ChatLine::new(ChatLineKind::Status);
                    line.push(
                        format!(" ! [{}]", held_words(r, status_short, pending_behind(r))),
                        held_ink(r.updated_at, now_ms),
                    );
                    timeline.push(line.fitted(width));
                }
            }
            if show_bodies {
                match body_view(
                    &item.message_id,
                    item.authorized_body.as_ref(),
                    item.detail_open,
                    show_bodies,
                    bodies,
                ) {
                    BodyView::Shown(body) => {
                        for row in wrap_words(body, width.saturating_sub(2).max(1)) {
                            let mut line = ChatLine::new(ChatLineKind::Message);
                            line.push("  ", ChatInk::Text);
                            line.push(row, ChatInk::Text);
                            timeline.push(line.fitted(width));
                        }
                    }
                    BodyView::Unavailable => {
                        timeline.push(dim_line("  body unavailable", width));
                    }
                    BodyView::Hidden => {}
                }
            }
        }
    } else if view_journal {
        // Transmission journal: one chronological line per delivery fact.
        if visible_rows.is_empty() {
            let mut empty = ChatLine::new(ChatLineKind::Status);
            empty.push("  No transmission records in journal", ChatInk::Dim);
            timeline.push(empty.fitted(width));
        }
        let body_width = width.saturating_sub(4).max(1);
        for row in &visible_rows {
            let time_str = format_time(row.ts, now_ms);
            let status = proven_status_label(row.mailbox, row.wake);

            let mut heading = ChatLine::new(ChatLineKind::Message);
            heading.push("  ", ChatInk::Accent);
            heading.push(format!("[{time_str}] "), ChatInk::Dim);
            heading.push(&row.sender_label, ChatInk::Role(row.sender_label.clone()));
            if let Some(pane) = row.sender.pane_id() {
                heading.push(format!(" ({pane})"), ChatInk::Dim);
            } else if row.sender.is_admin() {
                heading.push(" [admin]", ChatInk::Dim);
            } else if row.sender.is_headless() {
                heading.push(" (headless)", ChatInk::Dim);
            }
            heading.push(" → ", ChatInk::Dim);
            heading.push(
                &row.recipient_label,
                ChatInk::Role(row.recipient_label.clone()),
            );
            if let Some(pane) = row.recipient.pane_id() {
                heading.push(format!(" ({pane})"), ChatInk::Dim);
            }
            heading.push(" · ", ChatInk::Dim);
            heading.push(status, ChatInk::Accent);

            let used = heading.width();
            let id = row.message_id.as_str();
            let id_w = display_width(id);
            if used + 2 + id_w <= width {
                heading.push(" ".repeat(width - used - id_w), ChatInk::Text);
                heading.push(id, ChatInk::Dim);
            }
            let owner = LineOwner {
                message_id: row.message_id.clone(),
                targets: vec![row.target.clone()],
            };
            timeline.push(heading.owned(owner).fitted(width));

            if let Some(ref reply) = row.reply_to {
                let mut line = ChatLine::new(ChatLineKind::Message);
                line.push("   ↳ reply to ", ChatInk::Dim);
                line.push(reply.as_str(), ChatInk::Dim);
                timeline.push(line.fitted(width));
            }

            let detail_open =
                detail.is_some_and(|d| d.target().target.message_id == row.message_id);
            let authorized = detail.and_then(|d| {
                let loaded = d.loaded();
                if detail_open {
                    loaded.body.as_ref()
                } else {
                    loaded
                        .thread
                        .iter()
                        .find(|e| e.message_id == row.message_id.as_str())
                        .and_then(|e| e.body.as_ref())
                }
            });
            match body_view(
                &row.message_id,
                authorized,
                detail_open,
                show_bodies,
                bodies,
            ) {
                BodyView::Shown(body_text) => {
                    let show_subject = row.reply_to.is_none()
                        && row
                            .subject
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .is_some_and(|s| s != "Direct Message" && !body_text.starts_with(s));
                    if show_subject {
                        if let Some(ref subj) = row.subject {
                            for line_text in wrap_words(subj, body_width) {
                                let mut line = ChatLine::new(ChatLineKind::Message);
                                line.push("   ", ChatInk::Text);
                                line.push(line_text, ChatInk::Role(row.sender_label.clone()));
                                timeline.push(line.fitted(width));
                            }
                        }
                    }
                    for line_text in wrap_words(body_text, body_width) {
                        let mut line = ChatLine::new(ChatLineKind::Message);
                        line.push("   | ", ChatInk::Dim);
                        line.push(line_text, ChatInk::Text);
                        timeline.push(line.fitted(width));
                    }
                }
                BodyView::Unavailable => {
                    if let Some(ref subject) = row.subject {
                        for line_text in wrap_words(subject, body_width) {
                            let mut line = ChatLine::new(ChatLineKind::Message);
                            line.push("   ", ChatInk::Text);
                            line.push(line_text, ChatInk::Text);
                            timeline.push(line.fitted(width));
                        }
                    }
                    timeline.push(dim_line("   body unavailable", width));
                }
                BodyView::Hidden => {
                    if let Some(ref subject) = row.subject {
                        for line_text in wrap_words(subject, body_width) {
                            let mut line = ChatLine::new(ChatLineKind::Message);
                            line.push("   ", ChatInk::Text);
                            line.push(line_text, ChatInk::Text);
                            timeline.push(line.fitted(width));
                        }
                    }
                }
            }

            if let Some(ref cause) = row.cause {
                let mut line = ChatLine::new(ChatLineKind::Status);
                line.push(
                    format!("   ! cause: {}", attention_cause_label(*cause)),
                    ChatInk::Attention,
                );
                timeline.push(line.fitted(width));
            }
            if let Some(ref block) = row.pre_write_block {
                let mut line = ChatLine::new(ChatLineKind::Status);
                line.push(format!("   ! block: {}", block), ChatInk::Attention);
                timeline.push(line.fitted(width));
            }

            timeline.push(plain(ChatLineKind::Blank, "", width));
        }
    } else {
        // Group chat: one heading per message that carries everything a
        // reader scans for (who, to whom, the subject, the proven status,
        // the full id). Compact is the heading plus any hold that needs a
        // person; full adds the body, the thread and every recipient's
        // fact.
        let body_width = width.saturating_sub(3).max(1);
        let parts: Vec<HeadingParts<'_>> = items
            .iter()
            .map(|item| HeadingParts::new(item, now_ms))
            .collect();
        let level = heading_level(&parts, width);
        for (item, parts) in items.iter().zip(parts) {
            timeline.push(parts.line(width, level));
            let expanded = show_bodies || item.detail_open;
            if expanded {
                if let Some(ref reply) = item.reply_to {
                    let mut line = ChatLine::new(ChatLineKind::Message);
                    line.push("   ↳ reply to ", ChatInk::Dim);
                    line.push(reply.as_str(), ChatInk::Dim);
                    timeline.push(line.fitted(width));
                }
                for entry in &item.thread_history {
                    let entry_avatar = Avatar::from_label(&entry.sender_label);
                    let mut line = ChatLine::new(ChatLineKind::Message);
                    line.push("   ", ChatInk::Text);
                    line.push(
                        chip(&entry_avatar),
                        ChatInk::Avatar(entry.sender_label.clone()),
                    );
                    line.push(" ", ChatInk::Text);
                    line.push(
                        &entry.sender_label,
                        ChatInk::Role(entry.sender_label.clone()),
                    );
                    line.push("  ", ChatInk::Text);
                    line.push(format_time(entry.ts, now_ms), ChatInk::Dim);
                    timeline.push(line.fitted(width));
                    if let Some(ref b) = entry.body {
                        for row in wrap_words(b, width.saturating_sub(5).max(1)) {
                            let mut line = ChatLine::new(ChatLineKind::Message);
                            line.push("     ", ChatInk::Text);
                            line.push(row, ChatInk::Text);
                            timeline.push(line.fitted(width));
                        }
                    }
                }
                match body_view(
                    &item.message_id,
                    item.authorized_body.as_ref(),
                    item.detail_open,
                    show_bodies,
                    bodies,
                ) {
                    BodyView::Shown(body) => {
                        for row in wrap_words(body, body_width) {
                            let mut line = ChatLine::new(ChatLineKind::Message);
                            line.push("   ", ChatInk::Text);
                            line.push(row, ChatInk::Text);
                            timeline.push(line.fitted(width));
                        }
                    }
                    BodyView::Unavailable => {
                        timeline.push(dim_line("   body unavailable", width));
                    }
                    BodyView::Hidden => {}
                }
            }

            // Recipient delivery facts. A hold always gets its line: it is
            // what needs a person. The rest of a broadcast's recipients
            // only spell theirs out in the full view; a direct message's
            // one fact is already on its heading.
            for r in &item.recipients {
                let held = r.is_attention || r.cause.is_some();
                if !(held || (show_bodies && item.is_broadcast)) {
                    continue;
                }
                let status_label = proven_status_label(r.mailbox, r.wake);
                let (glyph, ink) = status_ink(r);
                let mut line = ChatLine::new(ChatLineKind::Status);
                line.push("    ", ChatInk::Accent);
                if item.is_broadcast {
                    line.push(&r.label, ChatInk::Role(r.label.clone()));
                    if let Some(pane) = r.recipient.pane_id() {
                        line.push(format!(" ({pane})"), ChatInk::Dim);
                    } else if r.recipient.is_admin() {
                        line.push(" [admin]", ChatInk::Dim);
                    } else if r.recipient.is_headless() {
                        line.push(" (headless)", ChatInk::Dim);
                    }
                    line.push(" ", ChatInk::Text);
                }
                line.push(glyph, ink.clone());
                line.push(" ", ChatInk::Text);
                line.push(status_label, ink);
                // The hold rides the status line it qualifies instead of
                // claiming a row of its own. Same words, same causes, one
                // line: a message used to spend two rows on transport for
                // every one row of what it said.
                if held {
                    line.push(" · ", ChatInk::Dim);
                    line.push(
                        held_words(r, status_label, pending_behind(r)),
                        held_ink(r.updated_at, now_ms),
                    );
                }
                timeline.push(line.owned(item.owner_of(r)).fitted(width));
            }
            if show_bodies {
                timeline.push(plain(ChatLineKind::Blank, "", width));
            }
        }
    }
    ChatTimeline { lines: timeline }
}

/// Everything a message heading is made of, measured once so one layout
/// can be chosen for the whole timeline before any row is laid out.
struct HeadingParts<'a> {
    item: &'a TimelineItem,
    /// The cursor cell, the sender chip and name, the arrow and the
    /// recipient(s): the part that never yields.
    who: ChatLine,
    who_w: usize,
    glyph: &'static str,
    label: &'static str,
    short: &'static str,
    ink: ChatInk,
    subject: Option<&'a str>,
    time: String,
}

impl<'a> HeadingParts<'a> {
    fn new(item: &'a TimelineItem, now_ms: Option<u64>) -> Self {
        let mut who = ChatLine::new(ChatLineKind::Message);
        who.push("  ", ChatInk::Accent);
        who.push(
            chip(&item.sender_avatar),
            ChatInk::Avatar(item.sender_label.clone()),
        );
        who.push(" ", ChatInk::Text);
        who.push(&item.sender_label, ChatInk::Role(item.sender_label.clone()));
        who.push(" → ", ChatInk::Dim);
        if item.is_broadcast {
            who.push("@all", ChatInk::Accent);
            who.push(format!(" ({})", item.recipients.len()), ChatInk::Dim);
        } else if let Some(r) = item.recipients.first() {
            who.push(&r.label, ChatInk::Role(r.label.clone()));
        }
        let who_w = who.width();

        // One fact for the whole message. A broadcast whose recipients
        // disagree says so rather than reporting the first one's fact as
        // everyone's.
        let (glyph, label, short, ink) = match item.recipients.first() {
            Some(first)
                if item
                    .recipients
                    .iter()
                    .all(|r| (r.mailbox, r.wake) == (first.mailbox, first.wake)) =>
            {
                let (glyph, ink) = status_ink(first);
                (
                    glyph,
                    proven_status_label(first.mailbox, first.wake),
                    proven_status_short(first.mailbox, first.wake),
                    ink,
                )
            }
            Some(_) => ("·", "mixed", "mixed", ChatInk::Dim),
            None => ("·", "no recipients", "none", ChatInk::Dim),
        };
        // The plain glyph is the separator's own dot; only a mark that
        // says something is worth a cell.
        let glyph = if glyph == "·" { "" } else { glyph };
        let subject = item
            .subject
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "Direct Message");
        HeadingParts {
            item,
            who,
            who_w,
            glyph,
            label,
            short,
            ink,
            subject,
            time: format_time(item.ts, now_ms),
        }
    }

    fn status(&self, form: StatusForm) -> String {
        let word = match form {
            StatusForm::Label => self.label,
            StatusForm::Short => self.short,
            StatusForm::Glyph => "",
        };
        match (self.glyph.is_empty(), word.is_empty()) {
            (true, _) => word.to_string(),
            (false, true) => self.glyph.to_string(),
            (false, false) => format!("{} {}", self.glyph, word),
        }
    }

    /// Cells one cascade level needs, the subject at its minimum.
    fn width_at(&self, level: usize) -> usize {
        let (form, with_id, with_time, with_subject) = HEADING_CASCADE[level];
        let sep = 3; // " · "
        let mut w = self.who_w;
        if with_subject && self.subject.is_some() {
            w += sep + SUBJECT_MIN;
        }
        let status = self.status(form);
        if !status.is_empty() {
            w += sep + display_width(&status);
        }
        if with_time {
            w += sep + display_width(&self.time);
        }
        if with_id {
            w += 2 + display_width(self.item.message_id.as_str());
        }
        w
    }

    /// The heading at one cascade level. What the level leaves over
    /// widens the subject.
    fn line(self, width: usize, level: usize) -> ChatLine {
        let (form, with_id, with_time, with_subject) = HEADING_CASCADE[level];
        let status = self.status(form);
        // Everything but the subject, which then gets the rest.
        let fixed = self.width_at(level)
            - if self.subject.is_some() && with_subject {
                SUBJECT_MIN
            } else {
                0
            };
        let item = self.item;
        let id = item.message_id.as_str();
        let mut heading = self.who;
        if let (true, Some(subject)) = (with_subject, self.subject) {
            let room = width.saturating_sub(fixed).min(display_width(subject));
            heading.push(" · ", ChatInk::Dim);
            heading.push(fit(subject, room), ChatInk::Text);
        }
        if !status.is_empty() {
            heading.push(" · ", ChatInk::Dim);
            heading.push(status, self.ink);
        }
        if with_time {
            heading.push(" · ", ChatInk::Dim);
            heading.push(self.time, ChatInk::Dim);
        }
        if with_id {
            let used = heading.width();
            heading.push(
                " ".repeat(width.saturating_sub(used + display_width(id))),
                ChatInk::Text,
            );
            heading.push(id, ChatInk::Dim);
        }
        heading.owned(item.owner()).fitted(width)
    }
}

/// The one cascade level every heading of the timeline fits at, so the
/// columns line up down the pane: a row that shows its id beside one
/// that shows its status instead is two layouts, not one compact view.
/// The last level when even that does not fit; [`ChatLine::fitted`]
/// cuts what is left.
fn heading_level(parts: &[HeadingParts<'_>], width: usize) -> usize {
    (0..HEADING_CASCADE.len())
        .find(|&level| parts.iter().all(|part| part.width_at(level) <= width))
        .unwrap_or(HEADING_CASCADE.len() - 1)
}

/// The least of a subject a heading shows: fewer cells than this tell a
/// reader nothing, so the row spends them on the id or status instead.
const SUBJECT_MIN: usize = 8;

/// How the heading names the proven status, largest first.
#[derive(Clone, Copy)]
enum StatusForm {
    Label,
    Short,
    Glyph,
}

/// The heading layouts in the order they are tried: (status form, id,
/// time, subject). Who never yields; the status shrinks from its proven
/// label to its short code to its glyph alone, then the time goes, then
/// the id, and a subject keeps at least [`SUBJECT_MIN`] cells whenever it
/// is shown at all, because a row an operator cannot tell from its
/// neighbours is not a compact row but a blank one.
const HEADING_CASCADE: [(StatusForm, bool, bool, bool); 10] = [
    (StatusForm::Label, true, true, true),
    (StatusForm::Label, true, false, true),
    (StatusForm::Short, true, false, true),
    (StatusForm::Glyph, true, false, true),
    (StatusForm::Short, false, false, true),
    (StatusForm::Glyph, false, false, true),
    (StatusForm::Label, true, false, false),
    (StatusForm::Short, true, false, false),
    (StatusForm::Short, false, false, false),
    (StatusForm::Glyph, false, false, false),
];

/// One dim line of plain words.
fn dim_line(text: &str, width: usize) -> ChatLine {
    let mut line = ChatLine::new(ChatLineKind::Message);
    line.push(text, ChatInk::Dim);
    line.fitted(width)
}

/// What the timeline shows under a message's heading for its body.
enum BodyView<'a> {
    Shown(&'a str),
    /// The operator asked for bodies and this one was refused, or the
    /// daemon does not offer the read.
    Unavailable,
    Hidden,
}

/// The body to show for one message.
///
/// 1. A body the daemon handed to the open detail (a claim) shows while
///    that detail is open, and whenever bodies are on.
/// 2. With bodies on, a body the operator read through `msg.read` shows;
///    a refused read says so in one line rather than showing nothing.
/// 3. Otherwise nothing: the UI never decides a body may be shown.
fn body_view<'a>(
    message_id: &MessageId,
    authorized: Option<&'a String>,
    detail_open: bool,
    show_bodies: bool,
    bodies: Option<&'a MessageBodies>,
) -> BodyView<'a> {
    if let Some(body) = authorized {
        if show_bodies || detail_open {
            return BodyView::Shown(body);
        }
    }
    if !show_bodies {
        return BodyView::Hidden;
    }
    match bodies.and_then(|bodies| bodies.get(message_id)) {
        Some(BodyState::Loaded(Some(body))) => BodyView::Shown(body),
        Some(BodyState::Unavailable) => BodyView::Unavailable,
        Some(BodyState::Loaded(None)) | Some(BodyState::Loading) | None => BodyView::Hidden,
    }
}

/// Paint one frame from a built timeline: header, the timeline window,
/// the status row and the composer or action strip, exactly `height` rows.
///
/// `scroll_top` is a window the operator moved by hand; `None` centers on
/// the selection. Either way the cursor cell of every line that speaks
/// for the selected target is painted here, so the timeline itself never
/// has to know what is selected.
pub fn render_chat_frame(
    timeline: &ChatTimeline,
    queue: &HumanQueue,
    context: ChatRenderContext<'_>,
    width: usize,
    height: usize,
    scroll_top: Option<usize>,
) -> ChatFrame {
    let ChatRenderContext {
        composer,
        status,
        retry_available,
        ..
    } = context;
    if width == 0 || height == 0 {
        return ChatFrame {
            lines: Vec::new(),
            viewport: ChatViewport::default(),
        };
    }

    let mut out: Vec<ChatLine> = Vec::with_capacity(height);
    let counts = queue.counts();
    let session_word = match queue.session_filter() {
        Some(filter) => format!("session {}", filter.name),
        None => "all sessions".to_string(),
    };

    // 1. Header line
    let hidden = queue.hidden_pending();
    let status_hint = status.unwrap_or("");
    let mut header = ChatLine::new(ChatLineKind::Header);
    if width >= 30 {
        header.push("Messages", ChatInk::Accent);
        header.push("  ", ChatInk::Text);
        header.push(queue.scope().word(), ChatInk::Text);
        header.push(" · ", ChatInk::Dim);
        header.push(&session_word, ChatInk::Text);
        header.push("  ", ChatInk::Text);
        if !status_hint.is_empty() {
            header.push(status_hint, hint_ink(status_hint));
        } else {
            let scope_hint = if hidden > 0 {
                " · press s for all"
            } else {
                ""
            };
            header.push(
                format!(
                    "{} shown · {} pend · {} attn{}",
                    counts.visible, counts.pending, counts.attention, scope_hint
                ),
                ChatInk::Dim,
            );
        }
    } else {
        header.push("Chat", ChatInk::Accent);
        header.push(" ", ChatInk::Text);
        if !status_hint.is_empty() {
            header.push(status_hint, hint_ink(status_hint));
        } else {
            header.push(
                format!("{} !{}", counts.pending, counts.attention),
                ChatInk::Dim,
            );
        }
    }
    out.push(header.fitted(width));

    // 2. Reserve the exact footer height before slicing the timeline. The
    // passive footer may wrap as the pane narrows, and every wrapped row is
    // part of the fixed chrome rather than expendable message space.
    let action_lines = if composer.is_some_and(|c| c.mode.is_some()) {
        Vec::new()
    } else {
        chat_action_lines(width, retry_available)
    };
    let composer_rows = if composer.is_some_and(|c| c.mode.is_some()) {
        4
    } else {
        1 + action_lines.len()
    };
    let status_rows = usize::from(status.is_some());
    let timeline_height = height.saturating_sub(1 + composer_rows + status_rows);

    // 3. The timeline window, cursor cells painted as it is copied.
    let selected = queue.selected().map(|row| &row.target);
    let total = timeline.lines.len();
    let top = match scroll_top {
        Some(top) => top.min(total.saturating_sub(timeline_height)),
        None => timeline_viewport_top(total, timeline_height, timeline.anchor_line(selected)),
    };
    for line in timeline.lines.iter().skip(top).take(timeline_height) {
        let mut line = line.clone();
        if let Some(selected) = selected {
            if line
                .owner
                .as_ref()
                .is_some_and(|owner| owner.targets.contains(selected))
            {
                line.mark_cursor();
            }
        }
        out.push(line);
    }
    while out.len() < 1 + timeline_height {
        out.push(plain(ChatLineKind::Blank, "", width));
    }

    // 4. Status row (if present)
    if let Some(status_text) = status {
        let mut line = ChatLine::new(ChatLineKind::Notice);
        line.push(status_text, hint_ink(status_text));
        out.push(line.fitted(width));
    }

    // 5. Bottom bounded composer, or the rule and the action strip.
    let rule = "─".repeat(width);
    match composer.and_then(|c| c.mode.as_ref().map(|mode| (c, mode))) {
        Some((c, mode)) => {
            let sender = match c.sender {
                Some(sender) if sender.is_admin() => "admin".to_string(),
                Some(sender) => sender
                    .pane_id()
                    .map(|pane| format!("agent:{pane}"))
                    .unwrap_or_else(|| sender.to_string()),
                None => "unavailable (read-only)".to_string(),
            };
            let mut mode_header = ChatLine::new(ChatLineKind::Composer);
            mode_header.push("── ", ChatInk::Dim);
            match mode {
                ComposerMode::Reply {
                    message_id,
                    origin_label,
                    ..
                } => {
                    mode_header.push("Reply to @", ChatInk::Accent);
                    mode_header.push(origin_label, ChatInk::Role(origin_label.clone()));
                    mode_header.push(format!(" ({message_id})"), ChatInk::Dim);
                }
                ComposerMode::Announce { recipients } => {
                    let preview = recipients
                        .iter()
                        .map(|(_, l)| l.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    mode_header.push("Announce to @all", ChatInk::Accent);
                    mode_header.push(format!(" ({preview})"), ChatInk::Dim);
                }
                ComposerMode::Direct {
                    recipient_label, ..
                } => {
                    mode_header.push("Direct to @", ChatInk::Accent);
                    mode_header.push(recipient_label, ChatInk::Role(recipient_label.clone()));
                }
            }
            mode_header.push(format!(" · sending as {sender} "), ChatInk::Dim);
            let used = mode_header.width();
            if used < width {
                mode_header.push("─".repeat(width - used), ChatInk::Dim);
            }
            out.push(mode_header.fitted(width));

            // Stage error / outcome banner if failed, not sent, or uncertain
            let stage_banner = match c.stage {
                Some(Stage::NotSent { ref why, .. }) => {
                    format!("! Not sent: {why} (draft preserved, Enter to retry)")
                }
                Some(Stage::Uncertain { ref why, .. }) => format!(
                    "! Uncertain: reconciliation required; draft preserved; outcome unconfirmed: {why}"
                ),
                Some(Stage::Failed { ref why, .. }) => {
                    format!("! Refused: {why} (draft preserved)")
                }
                Some(Stage::Acting(ref action)) => format!("... Sending {}...", action.word()),
                _ => String::new(),
            };
            if stage_banner.is_empty() {
                let mut input = ChatLine::new(ChatLineKind::Composer);
                input.push("> ", ChatInk::Accent);
                input.push(c.text(), ChatInk::Text);
                input.push("_", ChatInk::Dim);
                out.push(input.fitted(width));
            } else {
                let ink = if stage_banner.starts_with('!') {
                    ChatInk::Attention
                } else {
                    ChatInk::Dim
                };
                let mut banner = ChatLine::new(ChatLineKind::Composer);
                banner.push(stage_banner, ink);
                out.push(banner.fitted(width));
            }

            let mut help = ChatLine::new(ChatLineKind::Composer);
            help.push(
                "Enter send · Esc cancel · r reply · a announce",
                ChatInk::Dim,
            );
            out.push(help.fitted(width));
        }
        None => {
            let mut bar = ChatLine::new(ChatLineKind::Rule);
            bar.push(rule, ChatInk::Dim);
            out.push(bar.fitted(width));
            out.extend(action_lines);
        }
    }

    out.truncate(height);
    ChatFrame {
        lines: out,
        viewport: ChatViewport {
            top,
            visible: timeline_height,
            total,
        },
    }
}

#[cfg(test)]
mod strip_tests {
    use super::*;

    /// Every span the strip reports must land exactly on that verb's word
    /// in the row it rendered. This is the whole safety property of the
    /// clickable strip: a pointer lands on the verb it can read.
    #[test]
    fn every_reported_span_covers_exactly_its_own_verb() {
        let (row, spans) = chat_action_strip(80, false);
        // Counted against the verbs the pane binds, not against the list
        // the strip is built from: the two drifted once, and the strip
        // lost its session toggle without any test noticing.
        assert_eq!(
            spans
                .iter()
                .map(|(action, _, _)| *action)
                .collect::<Vec<_>>(),
            vec![
                ChatAction::Reply,
                ChatAction::Announce,
                ChatAction::Open,
                ChatAction::Body,
                ChatAction::Clear,
                ChatAction::Sessions,
            ],
            "{row}"
        );
        for (action, start, end) in spans {
            assert_eq!(
                &row[start..end],
                action.button(),
                "span for {action:?} must cover its own button in {row:?}"
            );
        }
    }

    /// The bar is centered: the same air on both sides of the buttons,
    /// give or take the odd cell, so it reads as a footer and not a caption.
    #[test]
    fn the_strip_is_centered_in_its_row() {
        let (row, spans) = chat_action_strip(80, false);
        let first = spans.first().map(|(_, start, _)| *start).unwrap();
        let last = spans.last().map(|(_, _, end)| *end).unwrap();
        assert_eq!(display_width(&row), 80);
        assert!(
            first.abs_diff(80 - last) <= 1,
            "lead {first} and trail {} must match: {row:?}",
            80 - last
        );
    }

    /// Every button span is inked as that button, so the surface can light
    /// exactly the control under the pointer.
    #[test]
    fn every_button_is_inked_as_its_own_verb() {
        let line = chat_action_line(80, true);
        let buttons: Vec<ChatAction> = line
            .spans
            .iter()
            .filter_map(|span| match span.ink {
                ChatInk::Button(action) => Some(action),
                _ => None,
            })
            .collect();
        assert_eq!(buttons, chat_actions(true));
    }

    #[test]
    fn words_wrap_on_spaces_and_only_cut_inside_an_overlong_word() {
        assert_eq!(
            wrap_words("the quick brown fox jumps", 10),
            vec!["the quick", "brown fox", "jumps"]
        );
        assert_eq!(
            wrap_words("abcdefghijkl end", 5),
            vec!["abcde", "fghij", "kl", "end"]
        );
        assert_eq!(wrap_words("one\n\ntwo", 10), vec!["one", "", "two"]);
        assert_eq!(
            wrap_words("wide 漢字 glyphs", 6),
            vec!["wide", "漢字", "glyphs"]
        );
    }

    /// A fitted line keeps the ink of every run that survives the cut and
    /// pads with plain cells, so the plain text is exactly the width.
    #[test]
    fn a_fitted_line_keeps_its_ink_and_its_width() {
        let mut line = ChatLine::new(ChatLineKind::Message);
        line.push("claude", ChatInk::Role("claude".into()));
        line.push(" → ", ChatInk::Dim);
        line.push("codex and more", ChatInk::Role("codex".into()));
        let fitted = line.clone().fitted(12);
        assert_eq!(fitted.text(), "claude → cod");
        assert_eq!(fitted.spans.len(), 3);
        assert_eq!(fitted.spans[2].ink, ChatInk::Role("codex".into()));
        let padded = line.fitted(30);
        assert_eq!(display_width(&padded.text()), 30);
        assert_eq!(padded.spans.last().unwrap().ink, ChatInk::Text);
    }

    /// Narrow panes wrap controls onto more footer rows instead of dropping
    /// the first verb that does not fit.
    #[test]
    fn narrow_footers_retain_every_action_in_order() {
        let strips = chat_action_strips(14, false);
        let actions = strips
            .iter()
            .flat_map(|(_, spans)| spans.iter().map(|(action, _, _)| *action))
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            chat_actions(false),
            "every footer action must survive wrapping: {strips:?}"
        );
        assert!(strips.len() > 1, "fourteen columns must exercise wrapping");
        for (row, spans) in strips {
            assert_eq!(display_width(&row), 14, "{row:?}");
            let first = spans.first().map(|(_, start, _)| *start).unwrap();
            let last = spans.last().map(|(_, _, end)| *end).unwrap();
            assert!(
                first.abs_diff(14 - last) <= 1,
                "each wrapped row stays centered: {row:?}"
            );
        }
    }

    /// Even below a button's natural width, the action retains its own row
    /// and clickable ink. The text is fitted because the cells do not exist,
    /// but the control does not vanish.
    #[test]
    fn an_ultranarrow_footer_does_not_drop_actions() {
        let actions = chat_action_lines(3, false)
            .iter()
            .flat_map(|line| {
                line.spans.iter().filter_map(|span| match span.ink {
                    ChatInk::Button(action) => Some(action),
                    _ => None,
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(actions, chat_actions(false));
    }

    /// The complete renderer reserves the wrapped footer before it slices
    /// the timeline, so the last rows cannot be truncated away by height.
    #[test]
    fn rendered_narrow_footer_keeps_every_action() {
        let registry = AvatarRegistry::default();
        let queue = HumanQueue::new();
        let rows = render_chat(&queue, ChatRenderContext::new(&registry), 14, 10);
        assert_eq!(rows.len(), 10);
        let rendered = rows.join("\n");
        for action in chat_actions(false) {
            assert!(
                rendered.contains(action.label()),
                "{action:?} must remain visible in the wrapped footer:\n{rendered}"
            );
        }
    }

    /// Retry is offered only when the caller owns a failed snapshot request,
    /// not merely because a passive daemon reconnect is underway.
    #[test]
    fn retry_appears_only_while_a_refresh_has_failed() {
        let (healthy, _) = chat_action_strip(80, false);
        assert!(!healthy.contains("retry"), "{healthy:?}");
        let (failed, spans) = chat_action_strip(80, true);
        assert!(
            failed.trim_start().starts_with(ChatAction::Retry.label()),
            "{failed:?}"
        );
        assert_eq!(spans[0].0, ChatAction::Retry);

        let registry = AvatarRegistry::default();
        let queue = HumanQueue::new();
        let reconnecting = render_chat(
            &queue,
            ChatRenderContext::new(&registry).with_status("daemon reconnecting"),
            80,
            10,
        )
        .join("\n");
        assert!(
            !reconnecting.contains(ChatAction::Retry.label()),
            "a passive reconnect must not advertise an operator retry"
        );
        let failed = render_chat(
            &queue,
            ChatRenderContext::new(&registry)
                .with_status("refresh failed: cyclopsd is unavailable")
                .with_retry_available(),
            80,
            10,
        )
        .join("\n");
        assert!(failed.contains(ChatAction::Retry.label()), "{failed}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Direction, QueueTarget, Scope, SessionFilter, Snapshot};
    use cyclops_proto::{
        MessageId, MessageRecipientRoute, NotificationAttentionCause, NotificationPreWriteCause,
        RecipientKey, SessionInstanceId,
    };

    trait FixtureParse: Sized {
        type Error;

        fn parse(value: &str) -> Result<Self, Self::Error>;
    }

    impl FixtureParse for RecipientKey {
        type Error = cyclops_proto::IdentityError;

        fn parse(value: &str) -> Result<Self, Self::Error> {
            value.parse()
        }
    }

    impl FixtureParse for MessageId {
        type Error = cyclops_proto::MailboxTypeError;

        fn parse(value: &str) -> Result<Self, Self::Error> {
            MessageId::new(value)
        }
    }

    fn make_test_queue() -> HumanQueue {
        let mut queue = HumanQueue::default();
        let s0 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%0",
        )
        .unwrap();
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let target1 = QueueTarget::new(m1.clone(), r1);
        let row1 = QueueRow {
            target: target1,
            message_id: m1.clone(),
            recipient: r1,
            recipient_label: "claude".into(),
            sender: s0,
            sender_label: "operator".into(),
            reply_to: None,
            thread_root: m1,
            thread_message_count: 1,
            ts: 1_000_000,
            kind: Kind::Msg,
            recipient_count: 1,
            subject: Some("Direct instruction to claude".into()),
            mailbox: MailboxWord::Pending,
            wake: WakeWord::Queued,
            fifo_position: Some(1),
            needs_action: true,
            can_manage_attention: true,
            can_withdraw_notification: true,
            seq: 1,
            updated_at: 1_000_000,
            direction: Direction::Inbound,
            ..Default::default()
        };

        // Broadcast message with 2 recipient rows (claude and codex)
        let r2 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%2",
        )
        .unwrap();
        let m2 = MessageId::parse("m-0000000000000002").unwrap();
        let target2_a = QueueTarget::new(m2.clone(), r1);
        let row2_a = QueueRow {
            target: target2_a,
            message_id: m2.clone(),
            recipient: r1,
            recipient_label: "claude".into(),
            sender: s0,
            sender_label: "operator".into(),
            reply_to: None,
            thread_root: m2.clone(),
            thread_message_count: 1,
            ts: 1_005_000,
            kind: Kind::Fyi,
            recipient_count: 2,
            subject: Some("Release broadcast".into()),
            mailbox: MailboxWord::Pending,
            wake: WakeWord::NotStarted,
            seq: 2,
            updated_at: 1_005_000,
            direction: Direction::Outbound,
            ..Default::default()
        };
        let target2_b = QueueTarget::new(m2.clone(), r2);
        let row2_b = QueueRow {
            target: target2_b,
            message_id: m2.clone(),
            recipient: r2,
            recipient_label: "codex".into(),
            sender: s0,
            sender_label: "operator".into(),
            reply_to: None,
            thread_root: m2,
            thread_message_count: 1,
            ts: 1_005_000,
            kind: Kind::Fyi,
            recipient_count: 2,
            subject: Some("Release broadcast".into()),
            mailbox: MailboxWord::Claimed,
            wake: WakeWord::Withdrawn,
            seq: 3,
            updated_at: 1_005_000,
            direction: Direction::Outbound,
            ..Default::default()
        };

        queue.replace(Snapshot {
            watermark: 3,
            rows: vec![row1, row2_a, row2_b],
        });
        queue.set_scope(Scope::All);
        queue
    }

    #[test]
    fn render_chat_distinguishes_direct_and_broadcast_by_structure() {
        let queue = make_test_queue();
        let reg = AvatarRegistry::default();
        let lines = render_chat(&queue, ChatRenderContext::new(&reg).at(1_010_000), 80, 20);

        let joined = lines.join("\n");
        assert!(
            joined.contains("operator → claude"),
            "a single-recipient message names its one recipient:\n{joined}"
        );
        assert!(
            joined.contains("operator → @all (2)"),
            "a multi-recipient broadcast is addressed to everyone:\n{joined}"
        );
        assert!(
            !joined.contains("[DIR]") && !joined.contains("[BC]"),
            "the direction is the address, not a tag:\n{joined}"
        );
        assert!(
            joined.contains(" OP "),
            "an unproven label must use initials"
        );
        assert!(
            !joined.contains('✳'),
            "an unproven claude label received an official icon"
        );
    }

    /// Narrowed to one session, the drawer shows the rows whose parties
    /// sit in that session's panes, counts only those, and names the
    /// session in its header; widened again, everything comes back.
    #[test]
    fn a_session_filter_narrows_rows_counts_and_the_header() {
        let mut queue = make_test_queue();
        let registry = AvatarRegistry::default();
        let all = queue.counts();
        assert_eq!(all.total, 3);

        // Every fixture row is addressed in this one session.
        let session: SessionInstanceId = "00000000-0000-0000-0000-000000000002".parse().unwrap();

        // %2 (codex) is the only pane in this session: the broadcast
        // reaches it, the direct message to claude (%1) does not.
        queue.set_session_filter(Some(SessionFilter::new(
            "beta",
            Some(session),
            ["%2".to_string()],
        )));
        let narrowed = queue.counts();
        assert_eq!(narrowed.total, 1, "one row has a party in %2");
        assert_eq!(narrowed.visible, 1);
        assert!(queue.visible().all(|row| row.recipient_label == "codex"));
        let frame = render_chat(&queue, ChatRenderContext::new(&registry), 80, 20).join("\n");
        assert!(frame.contains("session beta"), "{frame}");
        assert!(!frame.contains("Direct instruction"), "{frame}");

        // The sender's pane counts too: the operator sits in %0.
        queue.set_session_filter(Some(SessionFilter::new(
            "alpha",
            Some(session),
            ["%0".to_string()],
        )));
        assert_eq!(queue.counts().total, 3, "every row was sent from %0");

        queue.set_session_filter(None);
        assert_eq!(queue.counts(), all);
        let frame = render_chat(&queue, ChatRenderContext::new(&registry), 80, 20).join("\n");
        assert!(frame.contains("all sessions"), "{frame}");
    }

    /// tmux hands pane ids out again after a server restart, so the
    /// session that died and the one that replaced it both have a `%0`
    /// and a `%1`. Narrowed to the current session, the drawer shows
    /// only the rows addressed in it: a pane id alone admits nothing,
    /// and neither does an earlier recipient's live route through that
    /// pane. Widened again, the earlier session's history is still there.
    #[test]
    fn a_reused_pane_id_does_not_bring_an_earlier_sessions_messages_into_the_current_one() {
        let earlier: SessionInstanceId = "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let current: SessionInstanceId = "00000000-0000-0000-0000-000000000003".parse().unwrap();
        let key = |session: SessionInstanceId, pane: &str| {
            RecipientKey::parse(&format!(
                "agent:00000000-0000-0000-0000-000000000001/{session}/{pane}"
            ))
            .unwrap()
        };
        let route = |pane: &str| {
            Some(MessageRecipientRoute {
                label: "claude".into(),
                pane_id: Some(pane.parse().unwrap()),
            })
        };
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let m2 = MessageId::parse("m-0000000000000002").unwrap();
        let old_row = QueueRow {
            target: QueueTarget::new(m1.clone(), key(earlier, "%1")),
            message_id: m1.clone(),
            recipient: key(earlier, "%1"),
            recipient_label: "claude".into(),
            sender: key(earlier, "%0"),
            sender_label: "operator".into(),
            thread_root: m1,
            subject: Some("Before the restart".into()),
            // The old recipient's route still names the pane the new
            // session's agent now sits in.
            current_route: route("%1"),
            seq: 1,
            ..Default::default()
        };
        let new_row = QueueRow {
            target: QueueTarget::new(m2.clone(), key(current, "%1")),
            message_id: m2.clone(),
            recipient: key(current, "%1"),
            recipient_label: "claude".into(),
            sender: key(current, "%0"),
            sender_label: "operator".into(),
            thread_root: m2,
            subject: Some("After the restart".into()),
            current_route: route("%1"),
            seq: 2,
            ..Default::default()
        };
        let mut queue = HumanQueue::default();
        queue.replace(Snapshot {
            watermark: 2,
            rows: vec![old_row, new_row],
        });
        queue.set_scope(Scope::All);
        assert_eq!(queue.counts().total, 2);

        let panes = ["%0".to_string(), "%1".to_string()];
        queue.set_session_filter(Some(SessionFilter::new(
            "main",
            Some(current),
            panes.clone(),
        )));
        assert_eq!(
            queue.counts().total,
            1,
            "the earlier session's row shares every pane id and belongs to another session"
        );
        assert!(
            queue
                .visible()
                .all(|row| row.recipient == key(current, "%1")),
            "only the row addressed in the current session is shown"
        );

        // A session the daemon has not identified yet has no mailboxes,
        // so nothing can be addressed in it: the pane ids alone admit
        // nothing.
        queue.set_session_filter(Some(SessionFilter::new("main", None, panes)));
        assert_eq!(queue.counts().total, 0);

        // Durable history is untouched: every session shows both.
        queue.set_session_filter(None);
        assert_eq!(queue.counts().total, 2);
    }

    #[test]
    fn broadcast_selection_marks_the_exact_recipient() {
        let mut queue = make_test_queue();
        let message = MessageId::parse("m-0000000000000002").unwrap();
        let claude = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let codex = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%2",
        )
        .unwrap();
        let registry = AvatarRegistry::default();
        // The full view spells out every recipient of a broadcast; the
        // compact one keeps only the holds, so the per-recipient cursor
        // is a full-view fact.
        let context = || {
            ChatRenderContext::new(&registry)
                .at(1_010_000)
                .with_show_bodies(true)
        };

        assert!(queue.select(&QueueTarget::new(message.clone(), claude)));
        let claude_frame = render_chat(&queue, context(), 80, 20).join("\n");
        assert!(claude_frame.contains(">   claude"), "{claude_frame}");
        assert!(!claude_frame.contains(">   codex"), "{claude_frame}");
        let claude_narrow = render_chat(&queue, context(), 18, 14).join("\n");
        assert!(claude_narrow.contains(">-> claude"), "{claude_narrow}");
        assert!(claude_narrow.contains(" -> codex"), "{claude_narrow}");
        assert_eq!(queue.freeze().unwrap().target.recipient(), Some(claude));

        assert!(queue.select(&QueueTarget::new(message, codex)));
        let codex_frame = render_chat(&queue, context(), 80, 20).join("\n");
        assert!(codex_frame.contains(">   codex"), "{codex_frame}");
        assert!(!codex_frame.contains(">   claude"), "{codex_frame}");
        let codex_narrow = render_chat(&queue, context(), 18, 14).join("\n");
        assert!(codex_narrow.contains(">-> codex"), "{codex_narrow}");
        assert_eq!(queue.freeze().unwrap().target.recipient(), Some(codex));
    }

    /// The cursor is painted when the frame is cut, not when the timeline
    /// is built: one build serves every cursor position.
    #[test]
    fn the_cursor_moves_without_rebuilding_the_timeline() {
        let mut queue = make_test_queue();
        let registry = AvatarRegistry::default();
        let context = ChatRenderContext::new(&registry).at(1_010_000);
        let timeline = build_chat_timeline(&queue, context, 80);
        assert!(
            timeline
                .lines
                .iter()
                .all(|line| !line.text().starts_with('>')),
            "a built timeline carries no cursor"
        );
        let first = queue.selected().unwrap().target.clone();
        let before = render_chat_frame(&timeline, &queue, context, 80, 20, None);
        queue.select_next();
        assert_ne!(queue.selected().unwrap().target, first);
        let after = render_chat_frame(&timeline, &queue, context, 80, 20, None);
        let marked = |frame: &ChatFrame| {
            frame
                .lines
                .iter()
                .position(|line| line.text().starts_with('>'))
                .expect("one row carries the cursor")
        };
        assert_eq!(marked(&before) + 1, marked(&after));
        assert_eq!(
            timeline,
            build_chat_timeline(&queue, context, 80),
            "the selection did not change what was built"
        );
    }

    /// The wheel moves the window, and the window is clamped to the
    /// timeline, so a scroll past either end lands on that end.
    #[test]
    fn a_scrolled_window_is_clamped_to_the_timeline() {
        let queue = make_test_queue();
        let registry = AvatarRegistry::default();
        let context = ChatRenderContext::new(&registry)
            .at(1_010_000)
            .with_show_bodies(true);
        let timeline = build_chat_timeline(&queue, context, 80);
        let total = timeline.lines.len();
        let frame = render_chat_frame(&timeline, &queue, context, 80, 8, Some(usize::MAX));
        assert_eq!(frame.viewport.total, total);
        assert_eq!(frame.viewport.top, total - frame.viewport.visible);
        let frame = render_chat_frame(&timeline, &queue, context, 80, 8, Some(0));
        assert_eq!(frame.viewport.top, 0);
        assert_eq!(frame.lines.len(), 8);
    }

    #[test]
    fn thread_labels_never_claim_a_vendor_icon() {
        let mut queue = make_test_queue();
        let recipient = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let message = MessageId::parse("m-0000000000000001").unwrap();
        assert!(queue.select(&QueueTarget::new(message, recipient)));
        let mut detail = Detail::open(queue.selected().unwrap(), 3);
        detail.loaded_ok(crate::detail::Loaded {
            thread: vec![ThreadEntry {
                message_id: "m-history".into(),
                sender_label: "claude".into(),
                subject: Some("historical label only".into()),
                body: None,
                ts: 900_000,
            }],
            ..crate::detail::Loaded::default()
        });

        let frame = render_chat(
            &queue,
            ChatRenderContext::new(&AvatarRegistry::default())
                .with_detail(&detail)
                .at(1_010_000),
            80,
            20,
        )
        .join("\n");
        assert!(frame.contains(" CL  claude"), "{frame}");
        assert!(!frame.contains("✳"), "{frame}");
    }

    #[test]
    fn older_selection_stays_visible_at_narrow_and_wide_sizes() {
        let recipient = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let sender = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%0",
        )
        .unwrap();
        let mut rows = Vec::new();
        for index in 0..20u64 {
            let message = MessageId::parse(&format!("m-{index:016x}")).unwrap();
            rows.push(QueueRow {
                target: QueueTarget::new(message.clone(), recipient),
                message_id: message.clone(),
                recipient,
                recipient_label: "claude".into(),
                sender,
                sender_label: "operator".into(),
                thread_root: message,
                thread_message_count: 1,
                ts: 1_000_000 + index,
                kind: Kind::Msg,
                recipient_count: 1,
                subject: Some(format!("message {index}")),
                mailbox: MailboxWord::Pending,
                wake: WakeWord::NotStarted,
                needs_action: true,
                seq: index + 1,
                updated_at: 1_000_000 + index,
                direction: Direction::Inbound,
                ..QueueRow::default()
            });
        }
        let oldest = rows[0].target.clone();
        let mut queue = HumanQueue::default();
        queue.replace(Snapshot {
            watermark: 20,
            rows,
        });
        assert!(queue.select(&oldest));

        for (width, height) in [(18, 10), (80, 12)] {
            let frame = render_chat(
                &queue,
                ChatRenderContext::new(&AvatarRegistry::default()).at(1_010_000),
                width,
                height,
            );
            // Narrow rows carry the id's tail on the marked row; wide rows
            // carry the whole id on the marked heading.
            let selected_id = if width < 24 { "000000" } else { "m-00000000" };
            let marked = frame.iter().position(|line| line.starts_with('>'));
            assert!(
                marked.is_some_and(|at| frame[at..(at + 3).min(frame.len())]
                    .iter()
                    .any(|line| line.contains(selected_id))),
                "selected older message is outside the {width}x{height} viewport:\n{}",
                frame.join("\n")
            );
        }
    }

    #[test]
    fn proven_status_labels_never_collapse_to_delivered() {
        assert_eq!(
            proven_status_label(MailboxWord::Pending, WakeWord::NotStarted),
            "Accepted (wake not started)"
        );
        assert_eq!(
            proven_status_label(MailboxWord::Pending, WakeWord::Queued),
            "Wake queued"
        );
        assert_eq!(
            proven_status_label(MailboxWord::Claimed, WakeWord::Withdrawn),
            "Withdrawn"
        );
        assert_eq!(
            proven_status_label(MailboxWord::Claimed, WakeWord::NotStarted),
            "Claimed"
        );
        assert_eq!(
            proven_status_label(MailboxWord::Pending, WakeWord::NeedsAttention),
            "Attention"
        );
    }

    #[test]
    fn render_chat_narrow_width_fallback() {
        let queue = make_test_queue();
        let reg = AvatarRegistry::default();
        let lines = render_chat(&queue, ChatRenderContext::new(&reg).at(1_010_000), 18, 10);

        assert_eq!(lines.len(), 10);
        for line in &lines {
            assert!(
                display_width(line) <= 18,
                "line width {} exceeds 18: {:?}",
                display_width(line),
                line
            );
        }
    }

    #[test]
    fn reply_composer_binds_durable_id_and_endpoint() {
        let endpoint = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let message_id = MessageId::parse("m-0000000000000001").unwrap();
        let mut composer = ComposerState::new_reply(
            message_id,
            endpoint,
            "claude".into(),
            Some("Initial instruction".into()),
        );
        composer.push_char('A');
        composer.push_char('c');
        composer.push_char('k');

        assert_eq!(composer.text(), "Ack");
        let k1 = composer.key_for_send(|| "key-1".to_string());
        assert_eq!(k1, "key-1");
        // Re-asking key for unchanged draft reuses the same idempotent key
        let k2 = composer.key_for_send(|| "key-2".to_string());
        assert_eq!(k2, "key-1");
    }

    #[test]
    fn uncertain_stage_warns_reconciliation_required_without_enter_retry() {
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let mut composer = ComposerState::new_announce(vec![(r1, "claude".into())]);
        composer.push_char('H');
        composer.push_char('e');
        composer.push_char('y');

        composer.record_uncertain("send write completed but response timed out".into());
        assert_eq!(composer.text(), "Hey");
        assert!(matches!(composer.stage, Some(Stage::Uncertain { .. })));

        let reg = AvatarRegistry::default();
        let queue = make_test_queue();
        let lines = render_chat(
            &queue,
            ChatRenderContext::new(&reg)
                .with_composer(&composer)
                .at(1_010_000),
            80,
            10,
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains("Uncertain:"),
            "uncertain status banner must be displayed"
        );
        assert!(
            joined.contains("reconciliation required"),
            "uncertain stage must indicate reconciliation required"
        );
        assert!(
            !joined.contains("Uncertain: send write completed but response timed out (draft preserved, Enter to retry)"),
            "uncertain stage must never promise Enter to retry blindly"
        );
    }

    #[test]
    fn format_time_handles_ms_accurately() {
        let now = 1_000_000;
        assert_eq!(format_time(1_000_000, Some(now)), "just now");
        assert_eq!(format_time(995_000, Some(now)), "5s ago");
        assert_eq!(format_time(880_000, Some(now)), "2m ago");
        assert_eq!(format_time(0, Some(now)), "-");
    }

    #[test]
    fn stable_target_selection_under_insertions() {
        let mut queue = make_test_queue();
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let target1 = QueueTarget::new(m1.clone(), r1);

        queue.select(&target1);
        assert_eq!(
            queue.selected().map(|r| &r.target),
            Some(&target1),
            "target1 selected"
        );

        // Prepend a new row
        let r0 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%0",
        )
        .unwrap();
        let m0 = MessageId::parse("m-0000000000000000").unwrap();
        let row0 = QueueRow {
            target: QueueTarget::new(m0.clone(), r0),
            message_id: m0,
            recipient: r0,
            recipient_label: "worker".into(),
            sender: r0,
            sender_label: "operator".into(),
            subject: Some("New work".into()),
            mailbox: MailboxWord::Pending,
            wake: WakeWord::Queued,
            fifo_position: Some(1),
            needs_action: true,
            can_manage_attention: true,
            can_withdraw_notification: true,
            seq: 4,
            updated_at: 1_006_000,
            direction: Direction::Inbound,
            ..Default::default()
        };

        let mut rows = queue.visible().cloned().collect::<Vec<_>>();
        rows.insert(0, row0);
        queue.replace(Snapshot { watermark: 4, rows });

        // Selection remains on target1 by ID rather than positional index
        assert_eq!(
            queue.selected().map(|r| &r.target),
            Some(&target1),
            "target1 must remain selected by stable ID"
        );
    }

    #[test]
    fn reply_uses_the_referenced_message_without_a_mutable_selector() {
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let mode = ComposerMode::Reply {
            message_id: m1.clone(),
            origin_endpoint: r1,
            origin_label: "claude".into(),
            reply_subject: Some("Test".into()),
        };

        let route = mode.revalidate_routes(&[]).expect("valid reply");
        assert!(route.recipient_keys.is_none());
        assert!(!route.fyi);
        assert_eq!(route.reply_to, Some("m-0000000000000001".to_string()));
        assert_eq!(route.subject, "Re: m-0000000000000001");
    }

    #[test]
    fn revalidate_routes_rejects_missing_endpoint_and_never_retargets_by_label() {
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let r_different_session = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/99999999-9999-9999-9999-999999999999/%1",
        )
        .unwrap();
        let mode = ComposerMode::Direct {
            recipient_endpoint: r1,
            recipient_label: "claude".into(),
        };

        // Another pane has the same label name "claude", but a different session instance
        let live_routes = vec![cyclops_proto::StatusMailboxRoute {
            recipient: r_different_session,
            label: "claude".into(),
            unread: None,
        }];

        let result = mode.revalidate_routes(&live_routes);
        assert!(
            result.is_err(),
            "must refuse to retarget even if label matches"
        );
        let err_msg = result.unwrap_err();
        assert!(err_msg.contains("no longer live in mailbox routes"));
    }

    #[test]
    fn stale_connection_recovery_preserves_draft_and_idempotency_key() {
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let mut composer =
            ComposerState::new_reply(m1, r1, "claude".into(), Some("Refactor plan".into()));
        composer.push_char('S');
        composer.push_char('a');
        composer.push_char('f');
        composer.push_char('e');

        let key1 = composer.key_for_send(|| "idemp-key-100".to_string());
        assert_eq!(key1, "idemp-key-100");

        // Stale connection failure
        composer.record_not_sent("daemon connection reset".into());
        assert_eq!(composer.text(), "Safe");
        assert!(matches!(composer.stage, Some(Stage::NotSent { .. })));

        // Draft and client idempotency key are strictly preserved across reconnect/retry
        let key2 = composer.key_for_send(|| "idemp-key-999".to_string());
        assert_eq!(key2, "idemp-key-100");
        assert_eq!(composer.text(), "Safe");
    }

    #[test]
    fn held_queue_head_renders_verify_failed_and_accurate_behind_count() {
        let mut queue = make_test_queue();
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let m2 = MessageId::parse("m-0000000000000002").unwrap();
        let m3 = MessageId::parse("m-0000000000000003").unwrap();

        let head_row = QueueRow {
            target: QueueTarget::new(m1.clone(), r1),
            message_id: m1,
            recipient: r1,
            recipient_label: "blocked-worker".into(),
            sender: r1,
            sender_label: "operator".into(),
            subject: Some("Head message".into()),
            mailbox: MailboxWord::Pending,
            wake: WakeWord::NeedsAttention,
            cause: Some(NotificationAttentionCause::VerifyFailed),
            fifo_position: Some(1),
            needs_action: true,
            can_manage_attention: true,
            can_withdraw_notification: true,
            seq: 10,
            updated_at: 1_000_000,
            direction: Direction::Inbound,
            ..Default::default()
        };
        let row2 = QueueRow {
            target: QueueTarget::new(m2.clone(), r1),
            message_id: m2,
            recipient: r1,
            recipient_label: "blocked-worker".into(),
            sender: r1,
            sender_label: "operator".into(),
            subject: Some("Second message".into()),
            mailbox: MailboxWord::Pending,
            wake: WakeWord::Queued,
            fifo_position: Some(2),
            seq: 11,
            updated_at: 1_001_000,
            direction: Direction::Inbound,
            ..Default::default()
        };
        let row3 = QueueRow {
            target: QueueTarget::new(m3.clone(), r1),
            message_id: m3,
            recipient: r1,
            recipient_label: "blocked-worker".into(),
            sender: r1,
            sender_label: "operator".into(),
            subject: Some("Third message".into()),
            mailbox: MailboxWord::Pending,
            wake: WakeWord::Queued,
            fifo_position: Some(3),
            seq: 12,
            updated_at: 1_002_000,
            direction: Direction::Inbound,
            ..Default::default()
        };

        queue.replace(Snapshot {
            watermark: 12,
            rows: vec![head_row, row2, row3],
        });

        let reg = AvatarRegistry::default();
        let lines = render_chat(&queue, ChatRenderContext::new(&reg).at(1_010_000), 80, 25);

        let joined = lines.join("\n");
        assert!(
            joined.contains("head · held: verify failed · 2 behind"),
            "held queue head must render exact verify failed cause and behind count: {joined}"
        );
    }

    #[test]
    fn pre_write_held_head_renders_cause() {
        let mut queue = make_test_queue();
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let held_row = QueueRow {
            target: QueueTarget::new(m1.clone(), r1),
            message_id: m1,
            recipient: r1,
            recipient_label: "blocked-worker".into(),
            sender: r1,
            sender_label: "operator".into(),
            subject: Some("Pre-write block".into()),
            mailbox: MailboxWord::Pending,
            wake: WakeWord::BlockedBeforeWrite,
            pre_write_cause: Some(NotificationPreWriteCause::SessionUnavailable),
            fifo_position: Some(1),
            needs_action: true,
            can_manage_attention: true,
            can_withdraw_notification: true,
            seq: 10,
            updated_at: 1_000_000,
            direction: Direction::Inbound,
            ..Default::default()
        };
        queue.replace(Snapshot {
            watermark: 10,
            rows: vec![held_row],
        });

        let reg = AvatarRegistry::default();
        let lines = render_chat(&queue, ChatRenderContext::new(&reg).at(1_010_000), 80, 15);

        let joined = lines.join("\n");
        assert!(
            joined.contains("head · held: session unavailable"),
            "pre-write blocked head must render pre-write cause: {joined}"
        );
        assert!(
            joined.contains("Blocked before write"),
            "the resting surface must name the terminal-write boundary: {joined}"
        );
    }

    #[test]
    fn claimed_or_non_head_attention_row_does_not_label_head() {
        let mut queue = make_test_queue();
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let claimed_attention_row = QueueRow {
            target: QueueTarget::new(m1.clone(), r1),
            message_id: m1,
            recipient: r1,
            recipient_label: "claimed-agent".into(),
            sender: r1,
            sender_label: "operator".into(),
            subject: Some("Old claimed message with unretired alarm".into()),
            mailbox: MailboxWord::Claimed,
            wake: WakeWord::NeedsAttention,
            cause: Some(NotificationAttentionCause::VerifyFailed),
            fifo_position: None,
            needs_action: false,
            seq: 10,
            updated_at: 1_000_000,
            direction: Direction::Inbound,
            ..Default::default()
        };
        queue.replace(Snapshot {
            watermark: 10,
            rows: vec![claimed_attention_row],
        });

        let reg = AvatarRegistry::default();
        let lines = render_chat(&queue, ChatRenderContext::new(&reg).at(1_010_000), 80, 15);

        let joined = lines.join("\n");
        assert!(
            joined.contains("held: verify failed"),
            "claimed row must show cause: {joined}"
        );
        assert!(
            !joined.contains("head · held"),
            "claimed row must NOT be labelled as head: {joined}"
        );
    }

    #[test]
    fn render_chat_surfaces_status_hint_in_header() {
        let queue = make_test_queue();
        let reg = AvatarRegistry::default();
        let lines = render_chat(
            &queue,
            ChatRenderContext::new(&reg)
                .with_status("refresh failed · Ctrl+R to retry")
                .at(1_010_000),
            80,
            15,
        );

        let header = &lines[0];
        assert!(
            header.contains("refresh failed · Ctrl+R to retry"),
            "header must surface the link failure hint: {header}"
        );
    }

    /// The point of the collapse: a hold no longer buys its own row.
    ///
    /// Before this, one message spent two rows on transport state for
    /// every one row of what it actually said, and the screenshot that
    /// prompted the change was a wall of `! held:` with the subjects
    /// lost between them. The words are unchanged and still exact; they
    /// ride the status line they qualify.
    #[test]
    fn a_hold_rides_the_status_line_instead_of_taking_its_own_row() {
        let mut queue = make_test_queue();
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let row = QueueRow {
            target: QueueTarget::new(m1.clone(), r1),
            message_id: m1,
            recipient: r1,
            recipient_label: "held-agent".into(),
            sender: r1,
            sender_label: "operator".into(),
            subject: Some("Subject that must not be crowded out".into()),
            mailbox: MailboxWord::Claimed,
            wake: WakeWord::NeedsAttention,
            cause: Some(NotificationAttentionCause::VerifyFailed),
            fifo_position: None,
            needs_action: false,
            seq: 10,
            updated_at: 1_000_000,
            direction: Direction::Inbound,
            ..Default::default()
        };
        queue.replace(Snapshot {
            watermark: 10,
            rows: vec![row],
        });

        let reg = AvatarRegistry::default();
        let lines = render_chat_lines(&queue, ChatRenderContext::new(&reg).at(1_010_000), 80, 15);

        let carrying: Vec<&ChatLine> = lines
            .iter()
            .filter(|l| {
                l.spans
                    .iter()
                    .any(|s| s.text.contains("held: verify failed"))
            })
            .collect();
        assert_eq!(
            carrying.len(),
            1,
            "the cause must appear exactly once, not on a row of its own as well"
        );
        let text: String = carrying[0].spans.iter().map(|s| s.text.as_str()).collect();
        assert!(
            text.contains("Claimed") && text.contains("held: verify failed"),
            "the hold must sit on the same line as the delivery status it qualifies: {text}"
        );
        // Compact keeps the hold line: it is what needs a person. The
        // subject and the full id are on the heading above it.
        let heading = lines
            .iter()
            .position(|l| l.text().contains("held-agent"))
            .expect("the heading names the recipient");
        let heading_text = lines[heading].text();
        assert!(
            heading_text.contains("m-0000000000000001") && heading_text.contains("Subject that"),
            "{heading_text}"
        );
        assert!(lines[heading + 1].text().contains("held: verify failed"));
    }

    /// Decay is about attention, never about truth.
    ///
    /// A four-hour-old unresolved attempt rendered as loudly as one from
    /// fifteen seconds ago teaches an operator to ignore the colour, and
    /// then the fresh one means nothing. The words never change; only
    /// the ink does, and with no clock at all it stays loud.
    #[test]
    fn an_old_hold_keeps_its_words_and_loses_only_its_voice() {
        let fresh = 1_000_000u64;
        assert_eq!(
            held_ink(fresh, Some(fresh + HELD_LOUD_MS)),
            ChatInk::Attention,
            "a hold inside the loud window keeps full attention"
        );
        assert_eq!(
            held_ink(fresh, Some(fresh + HELD_LOUD_MS + 1)),
            ChatInk::Dim,
            "one millisecond past the window it stops competing"
        );
        assert_eq!(
            held_ink(fresh, None),
            ChatInk::Attention,
            "with no clock a renderer must not decide something is stale"
        );
    }

    #[test]
    fn attention_decay_follows_the_wake_update_not_message_age() {
        let now = 10_000_000u64;
        let mut row = QueueRow {
            subject: Some("Old message with a fresh failure".into()),
            mailbox: MailboxWord::Claimed,
            wake: WakeWord::NeedsAttention,
            cause: Some(NotificationAttentionCause::VerifyFailed),
            ts: now.saturating_sub(4 * HELD_LOUD_MS),
            updated_at: now - 1,
            seq: 10,
            ..Default::default()
        };

        let mut queue = make_test_queue();
        queue.replace(Snapshot {
            watermark: 10,
            rows: vec![row.clone()],
        });
        let reg = AvatarRegistry::default();
        let lines = render_chat_lines(&queue, ChatRenderContext::new(&reg).at(now), 100, 15);
        let fresh_hold = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.text.contains("held: verify failed"))
            .expect("fresh wake failure remains visible");
        assert_eq!(
            fresh_hold.ink,
            ChatInk::Attention,
            "an old message's newly changed wake must not start dim"
        );

        row.updated_at = now.saturating_sub(HELD_LOUD_MS + 1);
        queue.replace(Snapshot {
            watermark: 11,
            rows: vec![row],
        });
        let lines = render_chat_lines(&queue, ChatRenderContext::new(&reg).at(now), 100, 15);
        let old_hold = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.text.contains("held: verify failed"))
            .expect("old wake failure keeps its exact words");
        assert_eq!(old_hold.ink, ChatInk::Dim);

        let narrow_lines = render_chat_lines(&queue, ChatRenderContext::new(&reg).at(now), 23, 15);
        let narrow_hold = narrow_lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.text.contains("held: verify failed"))
            .expect("ultra-narrow mode keeps the old wake failure visible");
        assert_eq!(
            narrow_hold.ink,
            ChatInk::Dim,
            "ultra-narrow and normal layouts must use the same decay clock"
        );
    }

    #[test]
    fn authorized_bodies_widen_across_thread_and_render_multiline() {
        let mut queue = make_test_queue();
        let r1 = RecipientKey::parse(
            "agent:00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/%1",
        )
        .unwrap();
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let m2 = MessageId::parse("m-0000000000000002").unwrap();

        let row1 = QueueRow {
            target: QueueTarget::new(m1.clone(), r1),
            message_id: m1.clone(),
            recipient: r1,
            recipient_label: "reviewer".into(),
            sender: r1,
            sender_label: "coder".into(),
            subject: Some("Review implementation".into()),
            mailbox: MailboxWord::Claimed,
            wake: WakeWord::Submitted,
            seq: 10,
            updated_at: 1_000_000,
            direction: Direction::Inbound,
            ..Default::default()
        };
        let row2 = QueueRow {
            target: QueueTarget::new(m2.clone(), r1),
            message_id: m2.clone(),
            recipient: r1,
            recipient_label: "coder".into(),
            sender: r1,
            sender_label: "reviewer".into(),
            subject: Some("Re: Review implementation".into()),
            mailbox: MailboxWord::Claimed,
            wake: WakeWord::Submitted,
            seq: 11,
            updated_at: 1_001_000,
            direction: Direction::Inbound,
            ..Default::default()
        };

        queue.replace(Snapshot {
            watermark: 11,
            rows: vec![row1.clone(), row2.clone()],
        });

        // Detail is opened on row2, but its loaded thread contains row1's body!
        let mut detail = Detail::open(&row2, 11);
        detail.loaded_ok(crate::detail::Loaded {
            body: Some("Looks great, approved with one nit.\nPlease check line 42.".into()),
            body_authorized: true,
            thread: vec![ThreadEntry {
                message_id: m1.to_string(),
                sender_label: "coder".into(),
                subject: Some("Review implementation".into()),
                body: Some("Line 1 of implementation\nLine 2 of implementation".into()),
                ts: 1_000_000,
            }],
            ..Default::default()
        });

        let reg = AvatarRegistry::default();
        // Compact: the open detail's own body shows under its heading
        // because opening it was the ask; the thread's other body waits
        // for the full view.
        let compact = render_chat(
            &queue,
            ChatRenderContext::new(&reg)
                .with_detail(&detail)
                .at(1_010_000),
            80,
            25,
        )
        .join("\n");
        assert!(
            compact.contains("Looks great, approved with one nit."),
            "the open detail's body shows in the compact view: {compact}"
        );
        assert!(
            !compact.contains("Line 1 of implementation"),
            "a thread body stays folded in the compact view: {compact}"
        );

        let lines = render_chat(
            &queue,
            ChatRenderContext::new(&reg)
                .with_detail(&detail)
                .with_show_bodies(true)
                .at(1_010_000),
            80,
            25,
        );

        let text = lines.join("\n");
        // Both row1 (from thread entry) and row2 (from loaded body) must render their multi-line bodies!
        assert!(
            text.contains("Line 1 of implementation"),
            "row 1 body from thread entry must be rendered: {text}"
        );
        assert!(
            text.contains("Line 2 of implementation"),
            "row 1 line 2 must be rendered: {text}"
        );
        assert!(
            text.contains("Looks great, approved with one nit."),
            "row 2 body must be rendered: {text}"
        );
        assert!(
            text.contains("Please check line 42."),
            "row 2 line 2 must be rendered: {text}"
        );
    }
}
