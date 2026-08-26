//! Group-chat Messages experience: chat timeline, avatars, and bottom bounded composer.
//!
//! Pure data structures and renderers. Does not open sockets or issue IO directly.
//! Every timeline item displays strictly proven daemon facts without assuming wake or completion.

use std::collections::HashMap;

use cyclops_proto::{Kind, MessageId, RecipientKey};

use crate::avatar::{Avatar, AvatarRegistry};
use crate::detail::{Detail, Draft, Stage, ThreadEntry, DRAFT_MAX_BYTES};
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
            draft: Draft::default(),
            stage: None,
            focused: true,
        }
    }

    pub fn new_announce(recipients: Vec<(RecipientKey, String)>) -> Self {
        Self {
            mode: Some(ComposerMode::Announce { recipients }),
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

/// A recipient delivery fact entry on an aggregated message bubble.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientEntry {
    pub recipient: RecipientKey,
    pub label: String,
    pub avatar: Avatar,
    pub mailbox: MailboxWord,
    pub wake: WakeWord,
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

            let recip_entry = RecipientEntry {
                recipient: row.recipient,
                label: row.recipient_label.clone(),
                avatar: recip_avatar,
                mailbox: row.mailbox,
                wake: row.wake,
                is_attention: row.needs_human(),
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
                    if d.row.message_id == row.message_id {
                        if let Some(loaded) = d.loaded() {
                            (loaded.body.clone(), loaded.thread.clone())
                        } else {
                            (None, Vec::new())
                        }
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

/// Render the group-chat timeline and bottom bounded composer into exact-width lines.
pub fn render_chat(
    queue: &HumanQueue,
    detail: Option<&Detail>,
    composer: Option<&ComposerState>,
    avatar_registry: &AvatarRegistry,
    live_routes: Option<&[cyclops_proto::StatusMailboxRoute]>,
    pane_manifests: Option<&HashMap<String, String>>,
    width: usize,
    height: usize,
    status: Option<&str>,
    now_ms: Option<u64>,
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

    let mut timeline_lines: Vec<String> = Vec::new();

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
            let short_id = if item.message_id.as_str().len() > 6 {
                &item.message_id.as_str()[item.message_id.as_str().len() - 6..]
            } else {
                item.message_id.as_str()
            };
            let first_recip = item.recipients.first();
            let status_short = first_recip
                .map(|r| proven_status_short(r.mailbox, r.wake))
                .unwrap_or("*pend");

            let line1 = format!("{sel_mark}{attn_mark}[{s_initials}]{short_id} {status_short}");
            timeline_lines.push(fit(&line1, width));
            let recip_labels = item
                .recipients
                .iter()
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let line2 = format!(" -> {recip_labels}");
            timeline_lines.push(fit(&line2, width));
        }
    } else {
        // Full group-chat bubble mode
        for item in &items {
            let sel_mark = if item.is_selected { ">" } else { " " };
            let attn_mark = if item.recipients.iter().any(|r| r.is_attention) {
                "!"
            } else {
                " "
            };
            let s_badge = item.sender_avatar.badge();
            let time_str = format_time(item.ts, now_ms);

            let header = if item.is_broadcast {
                let recip_previews = item
                    .recipients
                    .iter()
                    .map(|r| format!("[{}] {}", r.avatar.badge(), r.label))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "{sel_mark}{attn_mark}[BC] [{s_badge}] {} -> @all ({time_str}) [{}]",
                    item.sender_label, item.message_id
                )
            } else if let Some(r) = item.recipients.first() {
                let r_badge = r.avatar.badge();
                format!(
                    "{sel_mark}{attn_mark}[DIR] [{s_badge}] {} -> [{r_badge}] {} ({time_str}) [{}]",
                    item.sender_label, r.label, item.message_id
                )
            } else {
                format!(
                    "{sel_mark}{attn_mark}[{s_badge}] {} ({time_str}) [{}]",
                    item.sender_label, item.message_id
                )
            };
            timeline_lines.push(fit(&header, width));

            if let Some(ref reply) = item.reply_to {
                timeline_lines.push(fit(&format!("   ↳ reply to {reply}"), width));
            }

            // Expose authorized thread history if present in detail
            for entry in &item.thread_history {
                let entry_avatar = avatar_registry.resolve(&entry.sender_label);
                let entry_time = format_time(entry.ts, now_ms);
                timeline_lines.push(fit(
                    &format!(
                        "   [{}] {} ({entry_time}):",
                        entry_avatar.badge(),
                        entry.sender_label
                    ),
                    width,
                ));
                if let Some(ref b) = entry.body {
                    for bline in b.lines() {
                        timeline_lines.push(fit(&format!("     {bline}"), width));
                    }
                }
            }

            // If message body is authorized, expose it; otherwise show metadata subject
            if let Some(ref body) = item.authorized_body {
                for line in body.lines() {
                    timeline_lines.push(fit(&format!("   {line}"), width));
                }
            } else if let Some(ref subj) = item.subject {
                timeline_lines.push(fit(&format!("   {subj}"), width));
            }

            // Recipient delivery truth states
            for r in &item.recipients {
                let status_label = proven_status_label(r.mailbox, r.wake);
                let states = format!(
                    "   [{}] [{status_label}] [mail: {}] [wake: {}]",
                    r.label,
                    r.mailbox.short(),
                    r.wake.short()
                );
                timeline_lines.push(fit(&states, width));
            }
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
                    origin_label,
                    ..
                } => format!("── Reply to @{origin_label} ({message_id}) ──"),
                ComposerMode::Announce { recipients } => {
                    let preview = recipients
                        .iter()
                        .map(|(_, l)| l.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("── Announce to @all ({preview}) ──")
                }
                ComposerMode::Direct {
                    recipient_label, ..
                } => {
                    format!("── Direct to @{recipient_label} ──")
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
                        format!("! [Uncertain: {why} (draft preserved; outcome unconfirmed, reconciliation required)]")
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
    use crate::queue::{Direction, QueueTarget, Snapshot};
    use cyclops_proto::{MessageId, RecipientKey};

    fn make_test_queue() -> HumanQueue {
        let mut queue = HumanQueue::default();
        let s0 = RecipientKey::parse(
            "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:%0",
        )
        .unwrap();
        let r1 = RecipientKey::parse(
            "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:%1",
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
            "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:%2",
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
        queue
    }

    #[test]
    fn render_chat_distinguishes_direct_and_broadcast_by_structure() {
        let queue = make_test_queue();
        let reg = AvatarRegistry::default();
        let lines = render_chat(
            &queue,
            None,
            None,
            &reg,
            None,
            None,
            80,
            20,
            None,
            Some(1_010_000),
        );

        let joined = lines.join("\n");
        assert!(
            joined.contains("[DIR]"),
            "single-recipient message must be marked [DIR]"
        );
        assert!(
            joined.contains("[BC]"),
            "multi-recipient broadcast must be marked [BC]"
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
        let lines = render_chat(
            &queue,
            None,
            None,
            &reg,
            None,
            None,
            18,
            10,
            None,
            Some(1_010_000),
        );

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
            "00000000-0000-0000-0000-000000000001:00000000-0000-0000-0000-000000000002:%1",
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
            None,
            Some(&composer),
            &reg,
            None,
            None,
            80,
            10,
            None,
            Some(1_010_000),
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
}
