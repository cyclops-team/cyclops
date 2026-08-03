//! Stream entries: one normalized record per daemon event or ledger line,
//! plus the two questions every view asks of one: is it admin-worthy, and
//! does it pass the current filter.

use cyclops_proto::{
    AgentState, AttentionItem, Clearance, DeliveryState, Event, Kind, LedgerLine, NotifyLevel,
    Resolved,
};
use serde_json::Value;

use crate::grid;
use crate::theme::Theme;

/// Columns before content: the 8-column HH:MM:SS gutter plus two spaces.
/// Continuation lines (bodies) hang at this indent, so the gutter stays a
/// clean aligned column whatever arrives.
pub const CONTENT_COL: usize = 10;

#[derive(Debug, Clone)]
pub struct Entry {
    /// Stable identity within one UI run, assigned by the app at push.
    /// Scroll anchors survive ring eviction through it.
    pub uid: u64,
    /// Unix ms.
    pub ts: u64,
    /// Ledger seq when the source was ledger-backed. Backfill dedupe key.
    pub seq: Option<u64>,
    /// The record id this entry belongs to, e.g. "m-3f9c2a": the message
    /// id on msg lines, and the same id again on the delivery and gate
    /// lines that advance it. It is what tells two deliveries to the same
    /// recipient apart. None only when the source carried no id.
    pub id: Option<String>,
    pub kind: EntryKind,
}

#[derive(Debug, Clone)]
pub enum EntryKind {
    Msg {
        from: String,
        to: Vec<String>,
        subject: String,
        body: Option<String>,
        fyi: bool,
    },
    Delivery {
        to: String,
        state: DeliveryState,
        cause: Option<String>,
    },
    Gate {
        to: String,
        action: String,
        detail: Option<String>,
    },
    /// One admin ping. It POINTS AT something that needs a human rather
    /// than being that thing, so the three names below say what: the pane
    /// whose blocked state raised it, the recipient of the delivery it is
    /// about (whose id is the entry's own `id`), or every delivery of a
    /// batch it summarizes. All absent means the ping names no register
    /// item, which is the operator's own `admin.notify` and anything a
    /// daemon older than the naming sends.
    Notify {
        level: NotifyLevel,
        subject: String,
        pane_id: Option<String>,
        to: Option<String>,
        deliveries: Vec<PingDelivery>,
    },
    State {
        target: String,
        pane_id: Option<String>,
        state: AgentState,
    },
    /// One thing that stopped needing a human, written the moment the
    /// register said so (`cyclops_proto::attention`, rule 3).
    ///
    /// Derived, not received: the daemon sends the transition, and the
    /// transition alone is ordinary traffic that says nothing about a
    /// human. This line is what carries that news into the calm view,
    /// beside the alarm row it answers, which stays exactly where it is.
    Cleared {
        was: AttentionItem,
        how: Clearance,
    },
    Session {
        name: String,
        text: String,
    },
    /// A pane left the tmux table. The pane's last transition, and the
    /// only thing that can drop its attention item while the process runs
    /// (`cyclops_proto::attention`): no state event ever arrives for a
    /// pane that is gone.
    PaneGone {
        pane_id: String,
    },
    Other {
        event: String,
        detail: Option<String>,
    },
}

/// One delivery a batch ping names, keyed the way the register keys it.
///
/// Named fields rather than a pair: both are strings, they sit next to
/// each other, and a transposed pair compiles and then matches nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingDelivery {
    /// Recipient, the register's first key.
    pub to: String,
    /// Message id, the register's second. Empty when the record carried
    /// none, which degrades to one slot per recipient exactly as
    /// `Attention::observe_delivery` does.
    pub id: String,
}

/// The `deliveries` list a batch ping carries in its data object:
/// `[{"to": "...", "id": "..."}]`. Absent on a ping about one item or
/// none, and on any daemon that predates it, both of which read as an
/// empty list.
fn ping_deliveries(d: &Value) -> Vec<PingDelivery> {
    let Some(Value::Array(items)) = d.get("deliveries") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|i| {
            let to = i.get("to").and_then(Value::as_str)?;
            Some(PingDelivery {
                to: to.to_string(),
                id: i
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
}

impl Entry {
    /// One live event from events.subscribe. Unknown vocabularies still
    /// render (as Other), never drop.
    pub fn from_event(ev: &Event, now_ms: u64) -> Entry {
        let d = &ev.data;
        let ts = d.get("ts").and_then(Value::as_u64).unwrap_or(now_ms);
        let kind = match ev.event.as_str() {
            "msg" => EntryKind::Msg {
                from: str_of(d, "from"),
                to: vec_of(d, "to"),
                subject: str_of(d, "subject"),
                body: opt_str(d, "body").filter(|b| !b.is_empty()),
                fyi: d.get("fyi").and_then(Value::as_bool).unwrap_or(false),
            },
            "delivery-state" => EntryKind::Delivery {
                to: str_of(d, "to"),
                state: state_of(d, "to_state"),
                cause: opt_str(d, "cause"),
            },
            "gate" => EntryKind::Gate {
                to: str_of(d, "to"),
                action: str_of(d, "action"),
                detail: opt_str(d, "cause").or_else(|| opt_str(d, "rule")),
            },
            "admin-notify" => EntryKind::Notify {
                level: serde_json::from_value(d.get("level").cloned().unwrap_or(Value::Null))
                    .unwrap_or(NotifyLevel::Fyi),
                subject: str_of(d, "subject"),
                pane_id: opt_str(d, "pane_id"),
                to: opt_str(d, "to"),
                deliveries: ping_deliveries(d),
            },
            "state" => EntryKind::State {
                target: str_of(d, "target"),
                pane_id: opt_str(d, "pane_id"),
                state: agent_state_of(d, "state"),
            },
            "session" => EntryKind::Session {
                name: str_of(d, "name"),
                text: session_text(d),
            },
            "pane-removed" => EntryKind::PaneGone {
                pane_id: str_of(d, "pane_id"),
            },
            other => EntryKind::Other {
                event: other.to_string(),
                detail: other_detail(d),
            },
        };
        Entry {
            uid: 0,
            ts,
            seq: ev.seq,
            id: opt_str(d, "id").filter(|i| !i.is_empty()),
            kind,
        }
    }

    /// One replayed ledger line, mapped onto the same stream vocabulary the
    /// live events use. Returns None only for lines that say nothing a
    /// stream reader can use (an empty data-less system line).
    pub fn from_ledger(line: &LedgerLine) -> Option<Entry> {
        let kind = match line.kind {
            Kind::Msg | Kind::Fyi => EntryKind::Msg {
                from: line.from.clone(),
                to: line.to.clone(),
                subject: line.subject.clone().unwrap_or_default(),
                body: line.body.clone().filter(|b| !b.is_empty()),
                fyi: line.kind == Kind::Fyi,
            },
            Kind::State => {
                let d = line.data.as_ref()?;
                // Delivery transitions carry to_state; fused agent states
                // carry state. The two share Kind::State on disk.
                if d.get("to_state").is_some() {
                    EntryKind::Delivery {
                        to: str_of(d, "to"),
                        state: state_of(d, "to_state"),
                        cause: opt_str(d, "cause"),
                    }
                } else {
                    EntryKind::State {
                        target: str_of(d, "target"),
                        pane_id: opt_str(d, "pane_id"),
                        state: agent_state_of(d, "state"),
                    }
                }
            }
            Kind::Gate => {
                let d = line.data.as_ref()?;
                EntryKind::Gate {
                    to: str_of(d, "to"),
                    action: str_of(d, "action"),
                    detail: opt_str(d, "cause").or_else(|| opt_str(d, "rule")),
                }
            }
            Kind::System => {
                let d = line.data.as_ref()?;
                match d.get("event").and_then(Value::as_str) {
                    Some("admin_notify") => EntryKind::Notify {
                        level: serde_json::from_value(
                            d.get("level").cloned().unwrap_or(Value::Null),
                        )
                        .unwrap_or(NotifyLevel::Fyi),
                        subject: line.subject.clone().unwrap_or_default(),
                        // The line's own `to` is the ping's audience
                        // (always admin); what it is ABOUT rides the data
                        // object beside the level.
                        pane_id: opt_str(d, "pane_id"),
                        to: opt_str(d, "to"),
                        deliveries: ping_deliveries(d),
                    },
                    Some("attach") => EntryKind::Session {
                        name: str_of(d, "session"),
                        text: "attached".into(),
                    },
                    Some("detach") => EntryKind::Session {
                        name: str_of(d, "session"),
                        text: "detached".into(),
                    },
                    Some("pane_labeled") => EntryKind::Session {
                        name: String::new(),
                        text: label_text(&str_of(d, "pane_id"), opt_str(d, "label").as_deref()),
                    },
                    Some(other) => EntryKind::Other {
                        event: other.to_string(),
                        detail: None,
                    },
                    None => return None,
                }
            }
        };
        Some(Entry {
            uid: 0,
            ts: line.ts,
            seq: Some(line.seq),
            id: Some(line.id.clone()).filter(|i| !i.is_empty()),
            kind,
        })
    }

    /// The line for one thing the register says stopped needing a human.
    ///
    /// `ts` is when it stopped, and `id` the record the transition carried,
    /// so the clearance sits in the stream at the moment it happened and
    /// under the same message id as the delivery it ends.
    pub fn cleared(ts: u64, id: Option<String>, resolved: Resolved) -> Entry {
        Entry {
            uid: 0,
            ts,
            seq: None,
            id,
            kind: EntryKind::Cleared {
                was: resolved.was,
                how: resolved.how,
            },
        }
    }

    /// The calm view (GOALS human layer): only what is aimed at the human
    /// plus the states that need one. Everything else waits in the firehose.
    ///
    /// "Needs one" is not decided here. Every branch that asks it calls
    /// `cyclops_proto::attention`: `delivery_needs_human` for a delivery,
    /// `AgentState::is_blocked` for a pane, `gate_cause_needs_human` for
    /// the gate's word for a pane. So the line the view admits and the
    /// item the eye counts can never be two different judgements.
    pub fn admin_visible(&self) -> bool {
        match &self.kind {
            EntryKind::Msg { to, .. } => to.iter().any(|t| t == "admin"),
            EntryKind::Delivery { state, .. } => cyclops_proto::delivery_needs_human(*state),
            // Most gate holds are the pipeline doing its job: the daemon
            // writes one hold line per cause per delivery, and queueing
            // behind a turn ("working") or behind a human in copy-mode
            // ("pane_in_mode") is routine traffic, not a thing to act on.
            // Only a hold on a blocked pane needs the human, and that is
            // the one the daemon also pings about. Every hold, routine or
            // not, still shows in the firehose.
            EntryKind::Gate { action, detail, .. } => {
                action == "hold"
                    && detail
                        .as_deref()
                        .is_some_and(cyclops_proto::gate_cause_needs_human)
            }
            // admin-notify is by definition aimed at the human: parked and
            // blocked pings, hook-unverified notices, restart closures.
            // A ping is a POINTER, though, so whether the thing it points
            // at still needs a human is the register's to answer and not
            // this line's: `App::admits` asks it before the calm view
            // takes one.
            EntryKind::Notify { .. } => true,
            EntryKind::State { state, .. } => state.is_blocked(),
            // The other half of the same judgement. A line reaches this
            // view because it says a human is needed, and the line saying
            // that is over belongs here for the same reason: the reader
            // watching the calm view is the one owed the ending. The
            // register decides there IS an ending (rule 3); by the time
            // one of these exists, it is admin-visible by construction.
            EntryKind::Cleared { .. } => true,
            // Roster churn: a pane arriving or leaving is not a thing to
            // act on. It moves the count, and the count's own line is the
            // blocked-state line that raised it.
            EntryKind::Session { .. } | EntryKind::PaneGone { .. } | EntryKind::Other { .. } => {
                false
            }
        }
    }

    /// The pane a focus jump should land on, when the entry names one.
    pub fn focus_target(&self) -> Option<&str> {
        match &self.kind {
            EntryKind::Msg { from, .. } => Some(from),
            EntryKind::Delivery { to, .. } | EntryKind::Gate { to, .. } => Some(to),
            EntryKind::State {
                pane_id, target, ..
            } => Some(pane_id.as_deref().unwrap_or(target)),
            // A clearance jumps where its alarm row jumped: the pane it
            // was about, or the delivery's recipient. Except when the pane
            // is the thing that went away, which is the same reason
            // PaneGone offers no jump: tmux has retired that id.
            EntryKind::Cleared {
                how: Clearance::PaneGone,
                ..
            } => None,
            EntryKind::Cleared { was, .. } => Some(match was {
                AttentionItem::Agent { pane_id, .. } => pane_id,
                AttentionItem::Delivery { to, .. } => to,
            }),
            // No jump for a pane that is gone; the notice would be the
            // whole answer, so say nothing instead.
            _ => None,
        }
    }

    /// Parties for filtering: (senders, recipients, everyone involved).
    fn parties(&self) -> (Vec<&str>, Vec<&str>, Vec<&str>) {
        match &self.kind {
            EntryKind::Msg { from, to, .. } => {
                let tos: Vec<&str> = to.iter().map(String::as_str).collect();
                let mut all = tos.clone();
                all.push(from);
                (vec![from], tos, all)
            }
            EntryKind::Delivery { to, .. } | EntryKind::Gate { to, .. } => {
                (vec![], vec![to.as_str()], vec![to.as_str()])
            }
            EntryKind::Notify { .. } => (vec![], vec!["admin"], vec!["admin"]),
            EntryKind::State { target, .. } => (vec![target.as_str()], vec![], vec![target]),
            // A clearance filters exactly as the row it answers does, or a
            // filter that admitted the alarm would hide its ending and put
            // the reader back where this whole rule started.
            EntryKind::Cleared { was, .. } => match was {
                AttentionItem::Agent { name, .. } => (vec![name.as_str()], vec![], vec![name]),
                AttentionItem::Delivery { to, .. } => (vec![], vec![to.as_str()], vec![to]),
            },
            EntryKind::Session { .. } | EntryKind::PaneGone { .. } | EntryKind::Other { .. } => {
                (vec![], vec![], vec![])
            }
        }
    }

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
                NotifyLevel::Fyi => format!("{} {sep} {subject}", theme.dim("fyi")),
                NotifyLevel::ActionRequired => format!("⚠ action required {sep} {subject}"),
                NotifyLevel::Urgent => format!("⚠ urgent {sep} {subject}"),
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
            EntryKind::PaneGone { pane_id } => theme.dim(&format!("{pane_id} closed")),
            EntryKind::Other { event, detail } => match detail {
                Some(d) => format!("{event}  {}", theme.dim(d)),
                None => event.clone(),
            },
        }
    }
}

/// Filters mirror the history flags: with is either direction, from and to
/// one direction each. All set filters must pass.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub with: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

impl Filter {
    pub fn is_empty(&self) -> bool {
        self.with.is_none() && self.from.is_none() && self.to.is_none()
    }

    pub fn matches(&self, e: &Entry) -> bool {
        if self.is_empty() {
            return true;
        }
        let (froms, tos, involved) = e.parties();
        if let Some(w) = &self.with {
            if !involved.iter().any(|p| p == w) {
                return false;
            }
        }
        if let Some(f) = &self.from {
            if !froms.iter().any(|p| p == f) {
                return false;
            }
        }
        if let Some(t) = &self.to {
            if !tos.iter().any(|p| p == t) {
                return false;
            }
        }
        true
    }

    /// Header words for the active filters, e.g. "with reviewer · from codex".
    pub fn words(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(w) = &self.with {
            parts.push(format!("with {w}"));
        }
        if let Some(f) = &self.from {
            parts.push(format!("from {f}"));
        }
        if let Some(t) = &self.to {
            parts.push(format!("to {t}"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        }
    }
}

fn str_of(d: &Value, key: &str) -> String {
    d.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn opt_str(d: &Value, key: &str) -> Option<String> {
    d.get(key).and_then(Value::as_str).map(String::from)
}

fn vec_of(d: &Value, key: &str) -> Vec<String> {
    match d.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn state_of(d: &Value, key: &str) -> DeliveryState {
    serde_json::from_value(d.get(key).cloned().unwrap_or(Value::Null))
        .unwrap_or(DeliveryState::Queued)
}

fn agent_state_of(d: &Value, key: &str) -> AgentState {
    serde_json::from_value(d.get(key).cloned().unwrap_or(Value::Null))
        .unwrap_or(AgentState::Unknown)
}

fn session_text(d: &Value) -> String {
    if let Some(attached) = d.get("attached").and_then(Value::as_bool) {
        return if attached { "attached" } else { "detached" }.into();
    }
    if let Some(pane) = d.get("pane_labeled").and_then(Value::as_str) {
        return label_text(pane, d.get("label").and_then(Value::as_str));
    }
    "changed".into()
}

fn label_text(pane: &str, label: Option<&str>) -> String {
    match label {
        Some(l) => format!("{pane} labeled {l}"),
        None => format!("{pane} unlabeled"),
    }
}

/// Compact detail for unknown event payloads: the JSON minus ts, mirroring
/// the CLI's watch fallback. None when nothing is left to say.
fn other_detail(d: &Value) -> Option<String> {
    let Value::Object(map) = d else {
        return None;
    };
    let rest: serde_json::Map<String, Value> = map
        .iter()
        .filter(|(k, _)| k.as_str() != "ts")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if rest.is_empty() {
        None
    } else {
        Some(Value::Object(rest).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn msg(from: &str, to: &[&str], subject: &str) -> Entry {
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

    fn ev(event: &str, data: Value) -> Event {
        Event {
            event: event.into(),
            data,
            seq: None,
        }
    }

    #[test]
    fn admin_stream_is_calm_firehose_is_everything() {
        // A message to admin is admin-visible; agent-to-agent is not.
        assert!(msg("codex", &["admin"], "done").admin_visible());
        assert!(msg("codex", &["admin", "reviewer"], "done").admin_visible());
        assert!(!msg("codex", &["reviewer"], "review this").admin_visible());

        // Attention deliveries surface; routine transitions stay quiet.
        let e = Entry::from_event(
            &ev(
                "delivery-state",
                json!({"to": "reviewer", "to_state": "attention_required"}),
            ),
            0,
        );
        assert!(e.admin_visible());
        let e = Entry::from_event(
            &ev(
                "delivery-state",
                json!({"to": "reviewer", "to_state": "parked_blocked_quota"}),
            ),
            0,
        );
        assert!(e.admin_visible());
        let e = Entry::from_event(
            &ev(
                "delivery-state",
                json!({"to": "reviewer", "to_state": "delivered_verified"}),
            ),
            0,
        );
        assert!(!e.admin_visible());

        // Blocked states surface; working does not.
        let e = Entry::from_event(
            &ev(
                "state",
                json!({"target": "reviewer", "state": "blocked_permission"}),
            ),
            0,
        );
        assert!(e.admin_visible());
        let e = Entry::from_event(
            &ev("state", json!({"target": "reviewer", "state": "working"})),
            0,
        );
        assert!(!e.admin_visible());

        // Holds on a blocked pane surface; routine holds and proceeds do
        // not. The daemon writes a hold line per cause per delivery, so
        // "working" is the ordinary queued-behind-a-turn path.
        let e = Entry::from_event(
            &ev(
                "gate",
                json!({"to": "reviewer", "action": "hold", "cause": "blocked:trust_dialog"}),
            ),
            0,
        );
        assert!(e.admin_visible());
        // The state's own name works too: the gate writes it for a quota.
        let e = Entry::from_event(
            &ev(
                "gate",
                json!({"to": "reviewer", "action": "hold", "cause": "blocked_quota"}),
            ),
            0,
        );
        assert!(e.admin_visible());
        // "session_detached" and "unknown" are causes the gate writes that
        // are nobody's to clear. The last two are the reason this asks
        // cyclops_proto instead of reading the first seven letters: a
        // prefix match called them blocked and put a calm eye directly
        // over a line saying a human was needed.
        for cause in [
            "working",
            "pane_in_mode",
            "idle_with_input",
            "session_detached",
            "unknown",
            "blockedfoo",
            "blocked_on_something_invented_next_year",
        ] {
            let e = Entry::from_event(
                &ev(
                    "gate",
                    json!({"to": "reviewer", "action": "hold", "cause": cause}),
                ),
                0,
            );
            assert!(!e.admin_visible(), "{cause} hold reached the calm view");
        }
        // A hold with no cause at all says nothing to act on.
        let e = Entry::from_event(&ev("gate", json!({"to": "reviewer", "action": "hold"})), 0);
        assert!(!e.admin_visible());
        let e = Entry::from_event(
            &ev(
                "gate",
                json!({"to": "reviewer", "action": "proceed", "rule": "prompt_visible"}),
            ),
            0,
        );
        assert!(!e.admin_visible());

        // Every admin ping surfaces (hook-unverified notices arrive here).
        let e = Entry::from_event(
            &ev(
                "admin-notify",
                json!({"level": "action_required", "subject": "hooks unverified on reviewer"}),
            ),
            0,
        );
        assert!(e.admin_visible());

        // Session churn is firehose-only.
        let e = Entry::from_event(&ev("session", json!({"name": "main", "attached": true})), 0);
        assert!(!e.admin_visible());
    }

    #[test]
    fn filters_mirror_the_history_flags() {
        let a = msg("codex", &["reviewer"], "s");
        let b = msg("reviewer", &["codex"], "s");
        let c = msg("admin", &["reviewer", "implementer"], "s");

        let with = Filter {
            with: Some("reviewer".into()),
            ..Filter::default()
        };
        assert!(with.matches(&a) && with.matches(&b) && with.matches(&c));
        let with_codex = Filter {
            with: Some("codex".into()),
            ..Filter::default()
        };
        assert!(with_codex.matches(&a) && with_codex.matches(&b) && !with_codex.matches(&c));

        let from = Filter {
            from: Some("codex".into()),
            ..Filter::default()
        };
        assert!(from.matches(&a) && !from.matches(&b));

        let to = Filter {
            to: Some("codex".into()),
            ..Filter::default()
        };
        assert!(!to.matches(&a) && to.matches(&b));

        // from plus to must both hold.
        let both = Filter {
            from: Some("codex".into()),
            to: Some("reviewer".into()),
            ..Filter::default()
        };
        assert!(both.matches(&a) && !both.matches(&b));

        // A state entry answers for its target in either direction.
        let st = Entry::from_event(
            &Event {
                event: "state".into(),
                data: json!({"target": "reviewer", "state": "working"}),
                seq: None,
            },
            0,
        );
        assert!(with.matches(&st));
        let from_reviewer = Filter {
            from: Some("reviewer".into()),
            ..Filter::default()
        };
        assert!(from_reviewer.matches(&st));
        assert!(!to.matches(&st));

        assert_eq!(
            Filter {
                with: Some("reviewer".into()),
                from: None,
                to: Some("codex".into())
            }
            .words()
            .as_deref(),
            Some("with reviewer · to codex")
        );
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

    #[test]
    fn ledger_lines_map_onto_the_stream_vocabulary() {
        let line: LedgerLine = serde_json::from_value(json!({
            "seq": 7, "boot_id": "b", "id": "m-aaa", "ts": 1000, "kind": "msg",
            "from": "codex", "to": ["reviewer"], "subject": "hi", "body": "b",
            "deliveries": []
        }))
        .unwrap();
        let e = Entry::from_ledger(&line).unwrap();
        assert_eq!(e.seq, Some(7));
        assert_eq!(e.id.as_deref(), Some("m-aaa"));
        assert!(matches!(e.kind, EntryKind::Msg { .. }));

        // A delivery transition and a fused state share Kind::State on disk.
        let line: LedgerLine = serde_json::from_value(json!({
            "seq": 8, "boot_id": "b", "id": "m-aaa", "ts": 1000, "kind": "state",
            "from": "cyclopsd", "to": ["reviewer"],
            "data": {"to": "reviewer", "from": "queued", "to_state": "gating"}
        }))
        .unwrap();
        let e = Entry::from_ledger(&line).unwrap();
        // The delivery line reuses the message id, which is what keys its
        // attention item back to the message it advances.
        assert_eq!(e.id.as_deref(), Some("m-aaa"));
        assert!(matches!(
            e.kind,
            EntryKind::Delivery {
                state: DeliveryState::Gating,
                ..
            }
        ));

        let line: LedgerLine = serde_json::from_value(json!({
            "seq": 9, "boot_id": "b", "id": "e-1", "ts": 1000, "kind": "state",
            "from": "cyclopsd",
            "data": {"target": "reviewer", "pane_id": "%1", "state": "working"}
        }))
        .unwrap();
        assert!(matches!(
            Entry::from_ledger(&line).unwrap().kind,
            EntryKind::State {
                state: AgentState::Working,
                ..
            }
        ));

        let line: LedgerLine = serde_json::from_value(json!({
            "seq": 10, "boot_id": "b", "id": "e-2", "ts": 1000, "kind": "system",
            "from": "cyclopsd", "to": ["admin"], "subject": "quota parked",
            "data": {"event": "admin_notify", "level": "urgent", "to": "implementer"}
        }))
        .unwrap();
        let e = Entry::from_ledger(&line).unwrap();
        assert!(e.admin_visible());
        // What the ping is ABOUT travels with the replayed line as well as
        // the live event, or the same ping would be held to the register
        // on the push and waved through on a restart. The line's own `to`
        // is the ping's audience (admin) and is not it.
        assert!(matches!(
            &e.kind,
            EntryKind::Notify {
                level: NotifyLevel::Urgent,
                pane_id: None,
                to: Some(to),
                ..
            } if to == "implementer"
        ));
    }

    /// The daemon stamps every delivery, gate, msg and notify event with
    /// the message id. Losing it collapses two deliveries to one
    /// recipient into one attention item.
    #[test]
    fn live_events_carry_the_record_id() {
        let e = Entry::from_event(
            &ev(
                "delivery-state",
                json!({"id": "m-bbb", "to": "reviewer", "to_state": "attention_required"}),
            ),
            0,
        );
        assert_eq!(e.id.as_deref(), Some("m-bbb"));
        let e = Entry::from_event(
            &ev(
                "msg",
                json!({"id": "m-bbb", "from": "codex", "to": ["reviewer"], "subject": "s"}),
            ),
            0,
        );
        assert_eq!(e.id.as_deref(), Some("m-bbb"));
        // An event without one stays an entry; nothing drops.
        let e = Entry::from_event(
            &ev("state", json!({"target": "reviewer", "state": "working"})),
            0,
        );
        assert_eq!(e.id, None);
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

    #[test]
    fn focus_targets_name_the_pane_side() {
        assert_eq!(
            msg("codex", &["reviewer"], "s").focus_target(),
            Some("codex")
        );
        let e = Entry::from_event(
            &ev(
                "state",
                json!({"target": "reviewer", "pane_id": "%4", "state": "idle"}),
            ),
            0,
        );
        assert_eq!(e.focus_target(), Some("%4"));
        let e = Entry::from_event(&ev("session", json!({"name": "main", "attached": true})), 0);
        assert_eq!(e.focus_target(), None);
    }
}
