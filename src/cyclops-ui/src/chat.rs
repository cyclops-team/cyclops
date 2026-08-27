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
    Scope,
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
            ChatAction::Scope => "s scope",
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
pub fn chat_actions(refresh_failed: bool) -> Vec<ChatAction> {
    let mut actions = vec![
        ChatAction::Reply,
        ChatAction::Announce,
        ChatAction::Open,
        ChatAction::Scope,
        ChatAction::Sessions,
    ];
    if refresh_failed {
        actions.insert(0, ChatAction::Retry);
    }
    actions
}

/// The strip as text, and where each verb sits in it.
///
/// Returns the rendered row plus one column span per button that actually
/// fit. A button the width could not hold is absent from the spans rather
/// than mapped to a truncated word, so a click can never land on half a
/// verb and dispatch the whole one. The spans cover the padded button, not
/// only its letters: the fill is the control, and its edge is clickable.
pub fn chat_action_strip(
    width: usize,
    refresh_failed: bool,
) -> (String, Vec<(ChatAction, usize, usize)>) {
    let line = chat_action_line(width, refresh_failed);
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
}

/// The strip as a styled line: every button that fits, centered in the row.
///
/// Centered because the strip is a bar of controls, not a sentence. Left
/// aligned it read as a caption under the timeline; centered it reads as
/// the footer of a panel, which is what it is.
pub fn chat_action_line(width: usize, refresh_failed: bool) -> ChatLine {
    let mut buttons: Vec<(ChatAction, String)> = Vec::new();
    let mut used = 0usize;
    for action in chat_actions(refresh_failed) {
        let button = action.button();
        let gap = if buttons.is_empty() {
            0
        } else {
            display_width(ACTION_GAP)
        };
        let w = display_width(&button);
        if used + gap + w > width {
            break;
        }
        used += gap + w;
        buttons.push((action, button));
    }
    let mut line = ChatLine::new(ChatLineKind::Strip);
    let lead = width.saturating_sub(used) / 2;
    line.push(" ".repeat(lead), ChatInk::Text);
    for (i, (action, button)) in buttons.into_iter().enumerate() {
        if i > 0 {
            line.push(ACTION_GAP, ChatInk::Text);
        }
        line.push(button, ChatInk::Button(action));
    }
    line.fitted(width)
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

/// One exact-width line of the drawer: inked runs that concatenate to the
/// plain row [`render_chat`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatLine {
    pub kind: ChatLineKind,
    pub spans: Vec<ChatSpan>,
}

impl ChatLine {
    pub fn new(kind: ChatLineKind) -> Self {
        Self {
            kind,
            spans: Vec::new(),
        }
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
        (_, WakeWord::NeedsAttention) => "Attention",
        (_, WakeWord::QuotaHeld) => "Quota held",
        (_, WakeWord::QuotaResetObserved) => "Quota reset",
        (_, WakeWord::BlockedBeforeWrite) => "Blocked before write",
        (_, WakeWord::ResolutionIncomplete) => "Resolution open",
        (_, WakeWord::Withdrawn) => "Withdrawn",
        (_, WakeWord::WithdrawnByOperator) => "Wake withdrawn",
        (_, WakeWord::Superseded) => "Superseded",
        (MailboxWord::Claimed, _) => "Claimed",
        (_, WakeWord::Queued) => "Wake queued",
        (_, WakeWord::Gating) => "Wake gating",
        (_, WakeWord::Writing) => "Wake writing",
        (_, WakeWord::Staged) => "Wake staged",
        (_, WakeWord::Submitted) => "Wake submit sent",
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
        (_, WakeWord::NeedsAttention) => "!attn",
        (_, WakeWord::QuotaHeld) => "!quota",
        (_, WakeWord::QuotaResetObserved) => "!reset",
        (_, WakeWord::BlockedBeforeWrite) => "!block",
        (_, WakeWord::ResolutionIncomplete) => "!incomp",
        (_, WakeWord::Withdrawn | WakeWord::WithdrawnByOperator) => "=wdrn",
        (_, WakeWord::Superseded) => "-sprsd",
        (_, WakeWord::Cleared | WakeWord::ResolvedDiscarded) => "xclear",
        (MailboxWord::Claimed, _) => "=claim",
        (_, WakeWord::Queued | WakeWord::Gating | WakeWord::Writing | WakeWord::Staged) => {
            ".wake-pend"
        }
        (_, WakeWord::Submitted | WakeWord::Notified | WakeWord::ResolvedSubmitted) => "^wake-sent",
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
    pub is_attention: bool,
    pub is_selected: bool,
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
    pub is_selected: bool,
    pub authorized_body: Option<String>,
    pub thread_history: Vec<ThreadEntry>,
}

impl TimelineItem {
    /// Aggregate QueueRows by MessageId into timeline entries.
    pub fn aggregate_from_rows(
        rows: &[QueueRow],
        avatar_registry: &AvatarRegistry,
        live_routes: Option<&[cyclops_proto::StatusMailboxRoute]>,
        pane_manifests: Option<&HashMap<String, String>>,
        selected_target: Option<&QueueTarget>,
        detail: Option<&Detail>,
    ) -> Vec<Self> {
        let mut grouped: Vec<TimelineItem> = Vec::new();
        let mut index_by_id: HashMap<MessageId, usize> = HashMap::new();

        for row in rows {
            let is_sel = selected_target == Some(&row.target);
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
                is_attention: row.needs_human(),
                is_selected: is_sel,
                target: row.target.clone(),
            };

            if let Some(&idx) = index_by_id.get(&row.message_id) {
                let item = &mut grouped[idx];
                item.recipients.push(recip_entry);
                if is_sel {
                    item.is_selected = true;
                }
                item.is_broadcast = item.recipients.len() > 1 || row.kind == Kind::Fyi;
            } else {
                let sender_avatar = avatar_registry.resolve_route_endpoint(
                    &row.sender,
                    &row.sender_label,
                    live_routes,
                    pane_manifests,
                );

                let is_broadcast = row.recipient_count > 1 || row.kind == Kind::Fyi;

                let (authorized_body, thread_history) = if let Some(d) = detail {
                    if d.target().target.message_id == row.message_id {
                        let loaded = d.loaded();
                        (loaded.body.clone(), loaded.thread.clone())
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
                    is_selected: is_sel,
                    authorized_body,
                    thread_history,
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
        }
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

    pub fn at(mut self, now_ms: u64) -> Self {
        self.now_ms = Some(now_ms);
        self
    }
}

/// The short form of a message id for a row: enough to tell two apart,
/// not the whole thirty-four cells. The full id is in the detail.
fn short_id(id: &MessageId) -> &str {
    let s = id.as_str();
    &s[..s.len().min(10)]
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

/// Render the group-chat timeline and bottom bounded composer into
/// exact-width inked lines.
pub fn render_chat_lines(
    queue: &HumanQueue,
    context: ChatRenderContext<'_>,
    width: usize,
    height: usize,
) -> Vec<ChatLine> {
    let ChatRenderContext {
        detail,
        composer,
        avatar_registry,
        live_routes,
        pane_manifests,
        status,
        retry_available,
        now_ms,
    } = context;
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let mut out: Vec<ChatLine> = Vec::with_capacity(height);
    let counts = queue.counts();
    let session_word = match queue.session_filter() {
        Some(filter) => format!("session {}", filter.name),
        None => "all sessions".to_string(),
    };

    // 1. Header line
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
            header.push(
                format!(
                    "{} shown · {} pend · {} attn",
                    counts.visible, counts.pending, counts.attention
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

    // Reserve rows for status and composer at the bottom
    let composer_rows = if composer.is_some_and(|c| c.mode.is_some()) {
        4
    } else {
        2
    };
    let status_rows = usize::from(status.is_some());
    let timeline_height = height.saturating_sub(1 + composer_rows + status_rows);

    // 2. Aggregate timeline rows by MessageId
    let visible_rows: Vec<QueueRow> = queue.visible().cloned().collect();
    let selected_target = queue.selected().map(|r| &r.target);
    let items = TimelineItem::aggregate_from_rows(
        &visible_rows,
        avatar_registry,
        live_routes,
        pane_manifests,
        selected_target,
        detail,
    );

    let mut timeline: Vec<ChatLine> = Vec::new();
    let mut selected_anchor_line: Option<usize> = None;
    let pending_behind = |entry: &RecipientEntry| {
        visible_rows
            .iter()
            .filter(|row| row.recipient == entry.recipient && row.mailbox == MailboxWord::Pending)
            .count()
            .saturating_sub(1)
    };

    if width < 24 {
        // Ultra-narrow mode: 1-2 lines per entry using initials (never icon only)
        for item in &items {
            let sel_mark = if item.is_selected { ">" } else { " " };
            let attn_mark = if item.recipients.iter().any(|r| r.is_attention) {
                "!"
            } else {
                " "
            };
            let s_initials = &item.sender_avatar.initials;
            let short = if item.message_id.as_str().len() > 6 {
                &item.message_id.as_str()[item.message_id.as_str().len() - 6..]
            } else {
                item.message_id.as_str()
            };
            let shown_recipient = item
                .recipients
                .iter()
                .find(|recipient| recipient.is_selected)
                .or_else(|| item.recipients.first());
            let status_short = shown_recipient
                .map(|r| proven_status_short(r.mailbox, r.wake))
                .unwrap_or("*pend");

            let line1 = format!("{sel_mark}{attn_mark}[{s_initials}]{short} {status_short}");
            timeline.push(plain(ChatLineKind::Message, &line1, width));
            let recip_labels = match item
                .recipients
                .iter()
                .find(|recipient| recipient.is_selected)
            {
                Some(recipient) if item.recipients.len() > 1 => {
                    format!(">{} +{}", recipient.label, item.recipients.len() - 1)
                }
                Some(recipient) => format!(">{}", recipient.label),
                None => item
                    .recipients
                    .iter()
                    .map(|recipient| recipient.label.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            };
            timeline.push(plain(
                ChatLineKind::Message,
                &format!(" -> {recip_labels}"),
                width,
            ));
            if item.is_selected {
                selected_anchor_line = Some(timeline.len().saturating_sub(1));
            }

            if let Some(r) = shown_recipient {
                if r.is_attention || r.cause.is_some() {
                    let is_head = r.mailbox == MailboxWord::Pending && r.fifo_position == Some(1);
                    let cause_desc = r.cause.as_deref().unwrap_or(status_short);
                    let line3 = if is_head {
                        let behind = pending_behind(r);
                        let behind_desc = if behind > 0 {
                            format!(" · {behind} behind")
                        } else {
                            String::new()
                        };
                        format!(" ! [head · held: {cause_desc}{behind_desc}]")
                    } else {
                        format!(" ! [held: {cause_desc}]")
                    };
                    let mut line = ChatLine::new(ChatLineKind::Status);
                    line.push(line3, ChatInk::Attention);
                    timeline.push(line.fitted(width));
                }
            }
        }
    } else {
        // Full group-chat mode: one heading, the wrapped body, one proven
        // fact per recipient.
        let body_width = width.saturating_sub(3).max(1);
        for item in &items {
            let mut heading = ChatLine::new(ChatLineKind::Message);
            heading.push(if item.is_selected { "> " } else { "  " }, ChatInk::Accent);
            heading.push(
                chip(&item.sender_avatar),
                ChatInk::Avatar(item.sender_label.clone()),
            );
            heading.push(" ", ChatInk::Text);
            heading.push(&item.sender_label, ChatInk::Role(item.sender_label.clone()));
            heading.push(" → ", ChatInk::Dim);
            if item.is_broadcast {
                heading.push("@all", ChatInk::Accent);
                heading.push(format!(" ({})", item.recipients.len()), ChatInk::Dim);
            } else if let Some(r) = item.recipients.first() {
                heading.push(chip(&r.avatar), ChatInk::Avatar(r.label.clone()));
                heading.push(" ", ChatInk::Text);
                heading.push(&r.label, ChatInk::Role(r.label.clone()));
            }
            let time = format_time(item.ts, now_ms);
            let used = heading.width();
            let time_w = display_width(&time);
            if used + 2 + time_w <= width {
                heading.push(" ".repeat(width - used - time_w), ChatInk::Text);
            } else {
                heading.push("  ", ChatInk::Text);
            }
            heading.push(time, ChatInk::Dim);
            timeline.push(heading.fitted(width));
            if item.is_selected {
                selected_anchor_line = Some(timeline.len().saturating_sub(1));
            }

            if let Some(ref reply) = item.reply_to {
                let mut line = ChatLine::new(ChatLineKind::Message);
                line.push("   ↳ reply to ", ChatInk::Dim);
                line.push(short_id(reply), ChatInk::Dim);
                timeline.push(line.fitted(width));
            }

            // Expose authorized thread history if present in detail
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

            // If message body is authorized, expose it; otherwise show metadata subject
            let words = item.authorized_body.as_deref().or(item.subject.as_deref());
            if let Some(words) = words {
                for row in wrap_words(words, body_width) {
                    let mut line = ChatLine::new(ChatLineKind::Message);
                    line.push("   ", ChatInk::Text);
                    line.push(row, ChatInk::Text);
                    timeline.push(line.fitted(width));
                }
            }

            // Recipient delivery truth states
            for r in &item.recipients {
                let status_label = proven_status_label(r.mailbox, r.wake);
                let (glyph, ink) = status_ink(r);
                let mut line = ChatLine::new(ChatLineKind::Status);
                if item.is_broadcast {
                    line.push(if r.is_selected { "  > " } else { "    " }, ChatInk::Accent);
                    line.push(&r.label, ChatInk::Role(r.label.clone()));
                    line.push(" ", ChatInk::Text);
                } else {
                    line.push("   ", ChatInk::Text);
                }
                line.push(glyph, ink.clone());
                line.push(" ", ChatInk::Text);
                line.push(status_label, ink);
                line.push(" · ", ChatInk::Dim);
                line.push(short_id(&item.message_id), ChatInk::Dim);
                timeline.push(line.fitted(width));
                if r.is_selected {
                    selected_anchor_line = Some(timeline.len().saturating_sub(1));
                }
                if r.is_attention || r.cause.is_some() {
                    let is_head = r.mailbox == MailboxWord::Pending && r.fifo_position == Some(1);
                    let cause_desc = r.cause.as_deref().unwrap_or(status_label);
                    let held = if is_head {
                        let behind = pending_behind(r);
                        let behind_desc = if behind > 0 {
                            format!(" · {behind} behind")
                        } else {
                            String::new()
                        };
                        format!("   ! head · held: {cause_desc}{behind_desc}")
                    } else {
                        let pos_desc = r
                            .fifo_position
                            .map(|pos| format!(" · pos {pos}"))
                            .unwrap_or_default();
                        format!("   ! held: {cause_desc}{pos_desc}")
                    };
                    let mut line = ChatLine::new(ChatLineKind::Status);
                    line.push(held, ChatInk::Attention);
                    timeline.push(line.fitted(width));
                }
            }
            timeline.push(plain(ChatLineKind::Blank, "", width));
        }
    }

    // Viewport slicing for timeline
    let total_lines = timeline.len();
    let start_line = timeline_viewport_top(total_lines, timeline_height, selected_anchor_line);
    out.extend(timeline.into_iter().skip(start_line).take(timeline_height));

    // Fill blank space if timeline is shorter than allocated height
    while out.len() < 1 + timeline_height {
        out.push(plain(ChatLineKind::Blank, "", width));
    }

    // 3. Status row (if present)
    if let Some(status_text) = status {
        let mut line = ChatLine::new(ChatLineKind::Notice);
        line.push(status_text, hint_ink(status_text));
        out.push(line.fitted(width));
    }

    // 4. Bottom Bounded Composer
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
            out.push(chat_action_line(width, retry_available));
        }
    }

    out.truncate(height);
    out
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
        assert_eq!(spans.len(), 5, "{row}");
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

    /// A verb the width cannot hold is absent rather than truncated: a
    /// half-printed word must never carry a whole action.
    #[test]
    fn a_verb_that_does_not_fit_has_no_span() {
        let (row, spans) = chat_action_strip(9, false);
        assert_eq!(
            spans.iter().map(|(a, _, _)| *a).collect::<Vec<_>>(),
            vec![ChatAction::Reply],
            "only the first verb fits in nine columns: {row:?}"
        );
        assert!(!row.contains("announce"), "{row:?}");
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
        MessageId, NotificationAttentionCause, NotificationPreWriteCause, RecipientKey,
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
            joined.contains("operator →  CL  claude"),
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
            joined.contains(" CL "),
            "an unproven claude label must use initials"
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

        // %2 (codex) is the only pane in this session: the broadcast
        // reaches it, the direct message to claude (%1) does not.
        queue.set_session_filter(Some(SessionFilter::new("beta", ["%2".to_string()])));
        let narrowed = queue.counts();
        assert_eq!(narrowed.total, 1, "one row has a party in %2");
        assert_eq!(narrowed.visible, 1);
        assert!(queue.visible().all(|row| row.recipient_label == "codex"));
        let frame = render_chat(&queue, ChatRenderContext::new(&registry), 80, 20).join("\n");
        assert!(frame.contains("session beta"), "{frame}");
        assert!(!frame.contains("Direct instruction"), "{frame}");

        // The sender's pane counts too: the operator sits in %0.
        queue.set_session_filter(Some(SessionFilter::new("alpha", ["%0".to_string()])));
        assert_eq!(queue.counts().total, 3, "every row was sent from %0");

        queue.set_session_filter(None);
        assert_eq!(queue.counts(), all);
        let frame = render_chat(&queue, ChatRenderContext::new(&registry), 80, 20).join("\n");
        assert!(frame.contains("all sessions"), "{frame}");
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

        assert!(queue.select(&QueueTarget::new(message.clone(), claude)));
        let claude_frame = render_chat(
            &queue,
            ChatRenderContext::new(&registry).at(1_010_000),
            80,
            20,
        )
        .join("\n");
        assert!(claude_frame.contains("  > claude"), "{claude_frame}");
        assert!(!claude_frame.contains("  > codex"), "{claude_frame}");
        let claude_narrow = render_chat(
            &queue,
            ChatRenderContext::new(&registry).at(1_010_000),
            18,
            10,
        )
        .join("\n");
        assert!(claude_narrow.contains("-> >claude +1"), "{claude_narrow}");
        assert_eq!(queue.freeze().unwrap().target.recipient(), Some(claude));

        assert!(queue.select(&QueueTarget::new(message, codex)));
        let codex_frame = render_chat(
            &queue,
            ChatRenderContext::new(&registry).at(1_010_000),
            80,
            20,
        )
        .join("\n");
        assert!(codex_frame.contains("  > codex"), "{codex_frame}");
        assert!(!codex_frame.contains("  > claude"), "{codex_frame}");
        let codex_narrow = render_chat(
            &queue,
            ChatRenderContext::new(&registry).at(1_010_000),
            18,
            10,
        )
        .join("\n");
        assert!(codex_narrow.contains("-> >codex +1"), "{codex_narrow}");
        assert_eq!(queue.freeze().unwrap().target.recipient(), Some(codex));
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
            // carry its head on the status row under the marked heading.
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
}
