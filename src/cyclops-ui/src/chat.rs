//! Group-chat Messages experience: chat timeline, avatars, and bottom bounded composer.
//!
//! Pure data structures and renderers. Does not open sockets or issue IO directly.
//! Every timeline item displays strictly proven daemon facts without assuming wake or completion.

use cyclops_proto::RecipientKey;

use crate::avatar::{Avatar, AvatarRegistry};
use crate::detail::{Draft, Stage, DRAFT_MAX_BYTES};
use crate::grid::display_width;
use crate::queue::{fit, HumanQueue, MailboxWord, QueueRow, WakeWord};

/// The mode and target context for the bottom bounded composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerMode {
    /// Replying to a specific durable message, bound to that message ID and sender endpoint.
    Reply {
        message_id: String,
        reply_to_label: String,
        origin_endpoint: Option<RecipientKey>,
        reply_subject: Option<String>,
    },
    /// Broadcasting an announcement expecting no reply, previewing resolved recipient labels.
    Announce { recipients_preview: Vec<String> },
    /// Direct message to a named recipient.
    Direct {
        recipient: String,
        recipient_endpoint: Option<RecipientKey>,
    },
}

impl ComposerMode {
    pub fn word(&self) -> &'static str {
        match self {
            ComposerMode::Reply { .. } => "Reply",
            ComposerMode::Announce { .. } => "Announce",
            ComposerMode::Direct { .. } => "Direct",
        }
    }
}

/// State of the bottom bounded composer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposerState {
    pub mode: Option<ComposerMode>,
    pub draft: Draft,
    pub stage: Option<Stage>,
    pub focused: bool,
}

impl ComposerState {
    pub fn new_reply(
        message_id: String,
        reply_to_label: String,
        origin_endpoint: Option<RecipientKey>,
        reply_subject: Option<String>,
    ) -> Self {
        Self {
            mode: Some(ComposerMode::Reply {
                message_id,
                reply_to_label,
                origin_endpoint,
                reply_subject,
            }),
            draft: Draft::default(),
            stage: None,
            focused: true,
        }
    }

    pub fn new_announce(recipients_preview: Vec<String>) -> Self {
        Self {
            mode: Some(ComposerMode::Announce { recipients_preview }),
            draft: Draft::default(),
            stage: None,
            focused: true,
        }
    }

    pub fn new_direct(recipient: String, recipient_endpoint: Option<RecipientKey>) -> Self {
        Self {
            mode: Some(ComposerMode::Direct {
                recipient,
                recipient_endpoint,
            }),
            draft: Draft::default(),
            stage: None,
            focused: true,
        }
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
}

/// Computes the exact proven delivery truth label from mailbox and wake states.
pub fn proven_status_label(mailbox: MailboxWord, wake: WakeWord) -> &'static str {
    match (mailbox, wake) {
        (_, WakeWord::NeedsAttention) => "Attention",
        (_, WakeWord::QuotaHeld) => "Quota held",
        (_, WakeWord::QuotaResetObserved) => "Quota reset",
        (_, WakeWord::BlockedBeforeWrite) => "Wake blocked",
        (_, WakeWord::ResolutionIncomplete) => "Resolution open",
        (_, WakeWord::Withdrawn) => "Withdrawn",
        (_, WakeWord::WithdrawnByOperator) => "Wake withdrawn",
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
        (MailboxWord::Pending, _) => "Accepted (pending)",
        (MailboxWord::DeliveredDirect, _) => "Delivered direct",
        (MailboxWord::Superseded, _) | (_, WakeWord::Superseded) => "Superseded",
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
        (MailboxWord::Claimed, _) => "=claim",
        (_, WakeWord::Queued | WakeWord::Gating | WakeWord::Writing | WakeWord::Staged) => {
            ".wake-pend"
        }
        (_, WakeWord::Submitted | WakeWord::Notified | WakeWord::ResolvedSubmitted) => "^wake-sent",
        (_, WakeWord::NotStarted) => "*acc-nostart",
        (MailboxWord::Pending, _) => "*acc-pend",
        (MailboxWord::DeliveredDirect, _) => "=dir",
        (MailboxWord::Superseded, _) | (_, WakeWord::Superseded) => "-sprsd",
        (_, WakeWord::Cleared | WakeWord::ResolvedDiscarded) => "xclear",
    }
}

/// A structured timeline message entry for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub message_id: String,
    pub is_broadcast: bool,
    pub sender_label: String,
    pub sender_avatar: Avatar,
    pub recipient_label: String,
    pub recipient_avatar: Avatar,
    pub subject: Option<String>,
    pub reply_to: Option<String>,
    pub ts: u64,
    pub mailbox: MailboxWord,
    pub wake: WakeWord,
    pub is_attention: bool,
    pub is_selected: bool,
}

impl TimelineItem {
    pub fn from_queue_row(
        row: &QueueRow,
        avatar_registry: &AvatarRegistry,
        is_selected: bool,
    ) -> Self {
        let sender_label = row
            .subject
            .as_deref()
            .and_then(|s| {
                if s.starts_with("from:") {
                    s.split_whitespace().next().map(|p| p[5..].to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "operator".to_string());

        let is_broadcast = row.recipient_label == "*"
            || row
                .subject
                .as_deref()
                .is_some_and(|s| s.contains("announcement") || s.contains("@all"));

        let sender_avatar = avatar_registry.resolve(&sender_label);
        let recipient_avatar = avatar_registry.resolve(&row.recipient_label);

        Self {
            message_id: row.message_id.to_string(),
            is_broadcast,
            sender_label,
            sender_avatar,
            recipient_label: row.recipient_label.clone(),
            recipient_avatar,
            subject: row.subject.clone(),
            reply_to: None,
            ts: row.updated_at,
            mailbox: row.mailbox,
            wake: row.wake,
            is_attention: row.needs_human(),
            is_selected,
        }
    }
}

/// Formats a relative timestamp or seconds display.
fn format_time(ts: u64) -> String {
    if ts == 0 {
        return "now".to_string();
    }
    format!("{ts}s")
}

/// Render the group-chat timeline and bottom bounded composer into exact-width lines.
pub fn render_chat(
    queue: &HumanQueue,
    composer: Option<&ComposerState>,
    avatar_registry: &AvatarRegistry,
    width: usize,
    height: usize,
    status: Option<&str>,
) -> Vec<String> {
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let mut out: Vec<String> = Vec::with_capacity(height);
    let counts = queue.counts();

    // 1. Header line
    if width >= 30 {
        out.push(fit(
            &format!(
                "Messages Group Chat  {}  {} shown  {} pend  {} attn",
                queue.scope().word(),
                counts.visible,
                counts.pending,
                counts.attention
            ),
            width,
        ));
    } else {
        out.push(fit(
            &format!("Chat {} !{}", counts.pending, counts.attention),
            width,
        ));
    }

    // Reserve rows for status and composer at the bottom
    let composer_rows = if composer.is_some_and(|c| c.mode.is_some()) {
        4
    } else {
        2
    };
    let status_rows = usize::from(status.is_some());
    let timeline_height = height.saturating_sub(1 + composer_rows + status_rows);

    // 2. Timeline rows
    let selected_target = queue.selected().map(|r| &r.target);
    let items: Vec<TimelineItem> = queue
        .visible()
        .map(|row| {
            let is_sel = selected_target == Some(&row.target);
            TimelineItem::from_queue_row(row, avatar_registry, is_sel)
        })
        .collect();

    let mut timeline_lines: Vec<String> = Vec::new();

    if width < 24 {
        // Ultra-narrow mode: 1-2 lines per entry
        for item in &items {
            let sel_mark = if item.is_selected { ">" } else { " " };
            let attn_mark = if item.is_attention { "!" } else { " " };
            let s_badge = item.sender_avatar.badge();
            let short_id = if item.message_id.len() > 6 {
                &item.message_id[item.message_id.len() - 6..]
            } else {
                &item.message_id
            };
            let status_short = proven_status_short(item.mailbox, item.wake);

            let line1 = format!("{sel_mark}{attn_mark}[{s_badge}]{short_id} {status_short}");
            timeline_lines.push(fit(&line1, width));
            let line2 = format!(" -> {}", item.recipient_label);
            timeline_lines.push(fit(&line2, width));
        }
    } else {
        // Full group-chat bubble mode
        for item in &items {
            let sel_mark = if item.is_selected { ">" } else { " " };
            let attn_mark = if item.is_attention { "!" } else { " " };
            let s_badge = item.sender_avatar.badge();
            let r_badge = item.recipient_avatar.badge();
            let status_label = proven_status_label(item.mailbox, item.wake);

            let header = if item.is_broadcast {
                format!(
                    "{sel_mark}{attn_mark}[BC] [{s_badge}] {} -> @all ({}) [{}]",
                    item.sender_label,
                    format_time(item.ts),
                    item.message_id
                )
            } else {
                format!(
                    "{sel_mark}{attn_mark}[DIR] [{s_badge}] {} -> [{r_badge}] {} ({}) [{}]",
                    item.sender_label,
                    item.recipient_label,
                    format_time(item.ts),
                    item.message_id
                )
            };
            timeline_lines.push(fit(&header, width));

            if let Some(ref reply) = item.reply_to {
                timeline_lines.push(fit(&format!("   ↳ reply to {reply}"), width));
            }

            if let Some(ref subj) = item.subject {
                timeline_lines.push(fit(&format!("   {subj}"), width));
            }

            let states = format!(
                "   [{status_label}] [mail: {}] [wake: {}]",
                item.mailbox.short(),
                item.wake.short()
            );
            timeline_lines.push(fit(&states, width));
            timeline_lines.push(fit("", width));
        }
    }

    // Viewport slicing for timeline
    let total_lines = timeline_lines.len();
    let start_line = total_lines.saturating_sub(timeline_height);
    for line in timeline_lines
        .into_iter()
        .skip(start_line)
        .take(timeline_height)
    {
        out.push(line);
    }

    // Fill blank space if timeline is shorter than allocated height
    while out.len() < 1 + timeline_height {
        out.push(fit("", width));
    }

    // 3. Status row (if present)
    if let Some(status_text) = status {
        out.push(fit(status_text, width));
    }

    // 4. Bottom Bounded Composer
    if let Some(c) = composer {
        if let Some(ref mode) = c.mode {
            let mode_header = match mode {
                ComposerMode::Reply {
                    message_id,
                    reply_to_label,
                    ..
                } => format!("── Reply to @{reply_to_label} ({message_id}) ──"),
                ComposerMode::Announce { recipients_preview } => {
                    let preview = recipients_preview.join(", ");
                    format!("── Announce to @all ({preview}) ──")
                }
                ComposerMode::Direct { recipient, .. } => {
                    format!("── Direct to @{recipient} ──")
                }
            };
            out.push(fit(&mode_header, width));

            // Stage error / outcome banner if failed, not sent, or uncertain
            if let Some(ref stage) = c.stage {
                let stage_banner = match stage {
                    Stage::NotSent { why, .. } => {
                        format!("! [Not sent: {why} (draft preserved, Enter to retry)]")
                    }
                    Stage::Uncertain { why, .. } => {
                        format!("! [Uncertain: {why} (draft preserved, Enter to retry)]")
                    }
                    Stage::Failed { why, .. } => {
                        format!("! [Refused: {why} (draft preserved)]")
                    }
                    Stage::Acting(action) => {
                        format!("... [Sending {}...]", action.word())
                    }
                    _ => String::new(),
                };
                if !stage_banner.is_empty() {
                    out.push(fit(&stage_banner, width));
                } else {
                    let input_line = format!("> {}_", c.text());
                    out.push(fit(&input_line, width));
                }
            } else {
                let input_line = format!("> {}_", c.text());
                out.push(fit(&input_line, width));
            }

            let help = "Enter send | Esc cancel | r reply | a announce";
            out.push(fit(help, width));
        } else {
            out.push(fit("── Messages Group Chat ──", width));
            out.push(fit("r reply | a announce | enter open | s scope", width));
        }
    } else {
        out.push(fit("── Messages Group Chat ──", width));
        out.push(fit("r reply | a announce | enter open | s scope", width));
    }

    out.truncate(height);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{QueueTarget, Snapshot};
    use cyclops_proto::{MessageId, RecipientKey};

    fn make_test_queue() -> HumanQueue {
        let mut queue = HumanQueue::default();
        let r1 = RecipientKey::parse(
            "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:%1",
        )
        .unwrap();
        let m1 = MessageId::parse("m-0000000000000001").unwrap();
        let target = QueueTarget::new(m1.clone(), r1);
        let row1 = QueueRow {
            target,
            message_id: m1,
            recipient: r1,
            recipient_label: "claude".into(),
            subject: Some("from:operator Initial instruction".into()),
            mailbox: MailboxWord::Pending,
            wake: WakeWord::Queued,
            cause: None,
            pre_write_cause: None,
            pre_write_pane_width: None,
            pre_write_required_pane_width: None,
            current_route: None,
            fifo_position: Some(1),
            needs_action: true,
            attention: None,
            resolution_intent: None,
            resolution_action_accepted: None,
            resolution_consumption_observed: None,
            can_manage_attention: true,
            can_withdraw_notification: true,
            seq: 1,
            updated_at: 100,
            direction: crate::queue::Direction::Inbound,
        };

        let r2 = RecipientKey::parse(
            "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:%2",
        )
        .unwrap();
        let m2 = MessageId::parse("m-0000000000000002").unwrap();
        let target2 = QueueTarget::new(m2.clone(), r2);
        let row2 = QueueRow {
            target: target2,
            message_id: m2,
            recipient: r2,
            recipient_label: "*".into(),
            subject: Some("from:operator Release announcement @all".into()),
            mailbox: MailboxWord::Pending,
            wake: WakeWord::NotStarted,
            cause: None,
            pre_write_cause: None,
            pre_write_pane_width: None,
            pre_write_required_pane_width: None,
            current_route: None,
            fifo_position: None,
            needs_action: false,
            attention: None,
            resolution_intent: None,
            resolution_action_accepted: None,
            resolution_consumption_observed: None,
            can_manage_attention: false,
            can_withdraw_notification: false,
            seq: 2,
            updated_at: 105,
            direction: crate::queue::Direction::Outbound,
        };

        queue.replace(Snapshot {
            watermark: 2,
            rows: vec![row1, row2],
        });
        queue
    }

    #[test]
    fn render_chat_distinguishes_direct_and_broadcast() {
        let queue = make_test_queue();
        let reg = AvatarRegistry::default();
        let lines = render_chat(&queue, None, &reg, 80, 20, None);

        let joined = lines.join("\n");
        assert!(
            joined.contains("[DIR]"),
            "direct message must be marked [DIR]"
        );
        assert!(
            joined.contains("[BC]"),
            "broadcast announcement must be marked [BC]"
        );
        assert!(joined.contains("[CC]"), "claude must have CC avatar badge");
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
        let lines = render_chat(&queue, None, &reg, 18, 10, None);

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
            "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:%1",
        )
        .unwrap();
        let mut composer = ComposerState::new_reply(
            "m-0000000000000001".into(),
            "claude".into(),
            Some(endpoint),
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
    fn draft_is_preserved_on_not_sent_and_uncertain() {
        let mut composer = ComposerState::new_announce(vec!["claude".into(), "codex".into()]);
        composer.push_char('H');
        composer.push_char('e');
        composer.push_char('y');

        composer.record_not_sent("daemon socket offline".into());
        assert_eq!(composer.text(), "Hey");
        assert!(matches!(composer.stage, Some(Stage::NotSent { .. })));

        composer.record_uncertain("send timed out".into());
        assert_eq!(composer.text(), "Hey");
        assert!(matches!(composer.stage, Some(Stage::Uncertain { .. })));
    }

    #[test]
    fn stable_target_selection_under_insertions() {
        let mut queue = make_test_queue();
        let r1 = RecipientKey::parse(
            "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:%1",
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
            "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:%0",
        )
        .unwrap();
        let m0 = MessageId::parse("m-0000000000000000").unwrap();
        let row0 = QueueRow {
            target: QueueTarget::new(m0.clone(), r0),
            message_id: m0,
            recipient: r0,
            recipient_label: "worker".into(),
            subject: Some("New work".into()),
            mailbox: MailboxWord::Pending,
            wake: WakeWord::Queued,
            cause: None,
            pre_write_cause: None,
            pre_write_pane_width: None,
            pre_write_required_pane_width: None,
            current_route: None,
            fifo_position: Some(1),
            needs_action: true,
            attention: None,
            resolution_intent: None,
            resolution_action_accepted: None,
            resolution_consumption_observed: None,
            can_manage_attention: true,
            can_withdraw_notification: true,
            seq: 3,
            updated_at: 110,
            direction: crate::queue::Direction::Inbound,
        };

        let mut rows = queue.visible().cloned().collect::<Vec<_>>();
        rows.insert(0, row0);
        queue.replace(Snapshot { watermark: 3, rows });

        // Selection remains on target1 by ID rather than positional index
        assert_eq!(
            queue.selected().map(|r| &r.target),
            Some(&target1),
            "target1 must remain selected by stable ID"
        );
    }
}
