//! Painting one [`crate::stream::Entry`]: the gutter, the content line,
//! and the body's first line hanging at the content column in comfortable
//! density.
//!
//! Everything about WHAT an entry says — its words, its glyphs, whether it
//! belongs in the calm view — is decided in `stream.rs` before this file
//! ever sees it. This file decides only how it looks: role color on names,
//! glyph-plus-word cells through [`crate::grid`], and nothing else. A
//! [`crate::theme::Theme`] with color off (`--plain`, `NO_COLOR`) renders
//! the same words with no escape byte, which is the whole of
//! `docs/development/INVARIANTS.md` rule 11: color is redundant here, never load-
//! bearing.

use crate::grid;
use crate::stream::{Entry, EntryKind};
use crate::theme::Theme;

/// Columns before content: the 8-column HH:MM:SS gutter plus two spaces.
/// Continuation lines (bodies) hang at this indent, so the gutter stays a
/// clean aligned column whatever arrives.
pub const CONTENT_COL: usize = 10;

impl Entry {
    /// The one or two rendered lines: gutter plus content, and the body's
    /// first line hanging at the content column when `body_line` asks for
    /// it (comfortable density). No trailing spaces, no truncation; the
    /// frame owns width.
    pub fn lines(&self, theme: &Theme, body_line: bool) -> Vec<String> {
        let clock = theme.dim(&grid::clock_hms(self.ts));
        let content = self.content(theme);
        let mut out = vec![format!("{clock}  {content}")];
        if body_line {
            if let EntryKind::Msg {
                body: Some(body), ..
            } = &self.kind
            {
                if let Some(first) = body.lines().next() {
                    out.push(format!("{}{first}", " ".repeat(CONTENT_COL)));
                }
            }
        }
        out
    }

    /// Content after the gutter, one line, the shared voice: role color on
    /// names, glyph+word for states, badges byte-equal to receipts.
    fn content(&self, theme: &Theme) -> String {
        let sep = theme.dim("·");
        match &self.kind {
            EntryKind::Msg {
                from,
                to,
                subject,
                fyi,
                ..
            } => {
                let who = match to.as_slice() {
                    [one] => format!("{} → {}", theme.role(from, from), theme.role(one, one)),
                    [] => theme.role(from, from),
                    many => format!("{} → {} agents", theme.role(from, from), many.len()),
                };
                let tag = if *fyi {
                    format!("  {}", theme.dim("fyi"))
                } else {
                    String::new()
                };
                format!("{who}{tag}  {subject}")
            }
            EntryKind::Delivery { to, state, cause } => {
                format!(
                    "{}  {}",
                    theme.role(to, to),
                    grid::delivery_badge(to, *state, cause.as_deref(), theme)
                )
            }
            EntryKind::Gate { to, action, detail } => {
                // A hold on a blocked pane is one of the three things that
                // reach the calm view, so it wears the same encoding as the
                // other two: glyph plus word, undimmed. The rule id that
                // named the prompt is manifest vocabulary and stays in the
                // record; the line says what happened and whose move it is.
                if self.admin_visible() {
                    return format!(
                        "{}  ⚠ held {sep} a prompt in this pane needs you",
                        theme.role(to, to)
                    );
                }
                let mut text = format!("gate {action}");
                if let Some(d) = detail {
                    text.push_str(&format!(" · {}", grid::cause_words(d)));
                }
                format!("{}  {}", theme.role(to, to), theme.dim(&text))
            }
            EntryKind::Notify { level, subject, .. } => match level {
                cyclops_proto::NotifyLevel::Fyi => {
                    format!("{} {sep} {subject}", theme.dim("fyi"))
                }
                cyclops_proto::NotifyLevel::ActionRequired => {
                    format!("⚠ action required {sep} {subject}")
                }
                cyclops_proto::NotifyLevel::Urgent => format!("⚠ urgent {sep} {subject}"),
            },
            EntryKind::State { target, state, .. } => {
                format!(
                    "{}  {}",
                    theme.role(target, target),
                    grid::state_cell(*state, theme)
                )
            }
            EntryKind::Cleared { was, how } => {
                format!(
                    "{}  {}",
                    theme.role(was.name(), was.name()),
                    grid::cleared_cell(was, *how, theme)
                )
            }
            EntryKind::Session { name, text } => {
                if name.is_empty() {
                    theme.dim(text)
                } else {
                    theme.dim(&format!("session {name} · {text}"))
                }
            }
            EntryKind::PaneGone {
                pane_id,
                physical_gone: true,
                ..
            } => theme.dim(&format!("{pane_id} closed")),
            EntryKind::PaneGone { pane_id, .. } => theme.dim(&format!("{pane_id} moved")),
            EntryKind::Other { event, detail } => match detail {
                Some(d) => format!("{event}  {}", theme.dim(d)),
                None => event.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{AgentState, AttentionItem, Clearance, DeliveryState, NotifyLevel};

    fn msg(from: &str, to: &[&str], subject: &str) -> Entry {
        Entry {
            uid: 0,
            ts: 43_471_000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Msg {
                from: from.into(),
                to: to.iter().map(|t| t.to_string()).collect(),
                subject: subject.into(),
                body: None,
                fyi: false,
            },
        }
    }

    #[test]
    fn entry_lines_are_exact_in_plain_mode() {
        let t = Theme::none();
        let e = msg("codex", &["reviewer"], "Review the rate limiter");
        assert_eq!(
            e.lines(&t, false),
            vec!["12:04:31  codex → reviewer  Review the rate limiter"]
        );

        let mut fyi = msg("admin", &["reviewer", "implementer"], "Standup in 5");
        if let EntryKind::Msg { fyi: f, .. } = &mut fyi.kind {
            *f = true;
        }
        assert_eq!(
            fyi.lines(&t, false),
            vec!["12:04:31  admin → 2 agents  fyi  Standup in 5"]
        );

        let mut with_body = msg("codex", &["reviewer"], "Review");
        if let EntryKind::Msg { body, .. } = &mut with_body.kind {
            *body = Some("gateway.rs:120 drops the burst path\nsecond".into());
        }
        assert_eq!(
            with_body.lines(&t, true),
            vec![
                "12:04:31  codex → reviewer  Review",
                "          gateway.rs:120 drops the burst path",
            ]
        );
        // Compact keeps the body off the grid.
        assert_eq!(with_body.lines(&t, false).len(), 1);

        let e = Entry {
            uid: 0,
            ts: 43_472_000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Delivery {
                to: "reviewer".into(),
                state: DeliveryState::DeliveredVerified,
                cause: None,
            },
        };
        assert_eq!(
            e.lines(&t, false),
            vec!["12:04:32  reviewer  ✔ delivered · verified"]
        );

        let e = Entry {
            uid: 0,
            ts: 43_473_000,
            seq: None,
            id: Some("e-1".into()),
            kind: EntryKind::State {
                target: "reviewer".into(),
                session_idx: 0,
                pane_id: Some("%1".into()),
                state: AgentState::BlockedPermission,
            },
        };
        assert_eq!(
            e.lines(&t, false),
            vec!["12:04:33  reviewer  ⚠ blocked_permission"]
        );

        let e = Entry {
            uid: 0,
            ts: 43_474_000,
            seq: None,
            id: Some("e-2".into()),
            kind: EntryKind::Notify {
                level: NotifyLevel::ActionRequired,
                subject: "delivery to reviewer needs attention".into(),
                pane_id: None,
                to: Some("reviewer".into()),
                deliveries: Vec::new(),
            },
        };
        assert_eq!(
            e.lines(&t, false),
            vec!["12:04:34  ⚠ action required · delivery to reviewer needs attention"]
        );

        let e = Entry {
            uid: 0,
            ts: 43_475_000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Gate {
                to: "reviewer".into(),
                action: "hold".into(),
                detail: Some("pane_in_mode".into()),
            },
        };
        assert_eq!(
            e.lines(&t, false),
            vec!["12:04:35  reviewer  gate hold · pane in mode"]
        );

        // The one gate line that reaches the calm view wears the same
        // encoding as the other two things that reach it: glyph plus word,
        // undimmed. The rule id that named the prompt is manifest
        // vocabulary and belongs in the record, not in this line.
        let e = Entry {
            uid: 0,
            ts: 43_475_000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Gate {
                to: "reviewer".into(),
                action: "hold".into(),
                detail: Some("blocked:trust_dialog".into()),
            },
        };
        assert!(e.admin_visible());
        let line = &e.lines(&t, false)[0];
        assert_eq!(
            line,
            "12:04:35  reviewer  ⚠ held · a prompt in this pane needs you"
        );
        assert!(!line.contains("trust_dialog"), "the rule id leaked: {line}");

        let e = Entry {
            uid: 0,
            ts: 43_476_000,
            seq: None,
            id: Some("e-3".into()),
            kind: EntryKind::Session {
                name: "main".into(),
                text: "attached".into(),
            },
        };
        assert_eq!(
            e.lines(&t, false),
            vec!["12:04:36  session main · attached"]
        );

        // The line that ends an alarm quotes the alarm's own cell, so the
        // reader matches the two rows without decoding a second encoding
        // for one item. The pane's other ending says which one it was:
        // nobody answered that prompt.
        let cleared = |how| Entry {
            uid: 0,
            ts: 43_477_000,
            seq: None,
            id: None,
            kind: EntryKind::Cleared {
                was: AttentionItem::Agent {
                    pane_id: "%1".into(),
                    name: "reviewer".into(),
                    state: AgentState::BlockedPermission,
                },
                how,
            },
        };
        assert_eq!(
            cleared(Clearance::Moved).lines(&t, false),
            vec!["12:04:37  reviewer  ✔ cleared · was ⚠ blocked_permission"]
        );
        assert_eq!(
            cleared(Clearance::PaneGone).lines(&t, false),
            vec!["12:04:37  reviewer  ✔ pane closed · was ⚠ blocked_permission"]
        );
        let e = Entry {
            uid: 0,
            ts: 43_477_000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Cleared {
                was: AttentionItem::Delivery {
                    to: "implementer".into(),
                    id: "m-1".into(),
                    state: DeliveryState::ParkedBlockedQuota,
                },
                how: Clearance::Moved,
            },
        };
        assert_eq!(
            e.lines(&t, false),
            vec!["12:04:37  implementer  ✔ cleared · was ⊘ parked · quota"]
        );
    }

    /// The stream paints its state cells and delivery badges through the
    /// theme's group tokens, the same tokens and the same composition the
    /// CLI's status grid and receipts use (`cyclops_ui::grid`, and the
    /// `Paint` impls on this crate's `Theme` and the CLI's `Style`).
    ///
    /// The stream used to render both cells bare while the CLI painted
    /// them against the same theme file, so one state read two ways
    /// depending on which surface a reader was looking at.
    #[test]
    fn the_stream_paints_state_cells_and_badges_through_their_group() {
        let (engine, warnings) = cyclops_theme::Theme::parse(
            concat!(
                "[state]\n",
                "needs_you = { hex = \"#010203\", c256 = 41 }\n",
                "[badge]\n",
                "terminal = { hex = \"#040506\", c256 = 42 }\n",
                "[surface]\n",
                "dim = { hex = \"#070809\", c256 = 43 }\n",
            ),
            "test",
        )
        .expect("parse");
        assert!(warnings.is_empty(), "{warnings:?}");
        let t = Theme::with_engine(engine, false);

        let blocked = Entry {
            uid: 0,
            ts: 43_471_000,
            seq: None,
            id: Some("e-1".into()),
            kind: EntryKind::State {
                target: "reviewer".into(),
                session_idx: 0,
                pane_id: Some("%1".into()),
                state: AgentState::BlockedPermission,
            },
        };
        let line = &blocked.lines(&t, false)[0];
        assert!(
            line.contains("\x1b[38;5;41m⚠ blocked_permission\x1b[0m"),
            "the state cell went unpainted: {line:?}"
        );

        let parked = Entry {
            uid: 0,
            ts: 43_471_000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Delivery {
                to: "implementer".into(),
                state: DeliveryState::ParkedBlockedQuota,
                cause: None,
            },
        };
        let line = &parked.lines(&t, false)[0];
        assert!(
            line.contains("\x1b[38;5;42m⊘ parked \x1b[38;5;43m·\x1b[0m \x1b[38;5;43mquota"),
            "the badge went unpainted: {line:?}"
        );

        // Redundant, never alone (GOALS): with color off the same two
        // lines are the words and not one escape byte.
        let plain = Theme::none();
        assert_eq!(
            blocked.lines(&plain, false)[0],
            "12:04:31  reviewer  ⚠ blocked_permission"
        );
        assert_eq!(
            parked.lines(&plain, false)[0],
            "12:04:31  implementer  ⊘ parked · quota"
        );
    }
}
