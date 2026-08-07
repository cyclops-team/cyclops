//! The record: one normalized [`Entry`] per daemon event or ledger line,
//! the order backfill and the live push have to land in, and the two
//! judgements every reader of the record needs answered the same way
//! everywhere it is read.
//!
//! Backend-neutral on purpose: nothing here holds a socket, a terminal, or
//! a color. `cyclops watch` (app.rs, frame.rs, entry.rs, plain.rs) is the
//! first renderer of it; a workspace event panel is meant to be the
//! second, and both should read this module rather than keep a private
//! copy of any of the five things it owns:
//!
//! - initial backfill and live `events.subscribe` ordering ([`Intake`]);
//! - entry normalization and resolution rows ([`Entry::from_event`],
//!   [`Entry::from_ledger`], [`Entry::cleared`] — the append-only second
//!   line an alarm gets when it ends, `docs/development/INVARIANTS.md` rule 8);
//! - the calm/firehose decision the default Watch view uses
//!   ([`Entry::admin_visible`], [`Record::admits`]);
//! - semantic badges, state words, timestamps and user-facing copy, via
//!   [`crate::grid`] (color is a renderer's job and never reaches here —
//!   nothing in this file imports a theme);
//! - stable row identity for incremental updates ([`Entry::uid`], handed
//!   out once by [`Record`] and never reused within a run).
//!
//! [`Record`] is the ring-and-register half: it holds the entry ring, the
//! attention register (`cyclops_proto::attention`), and the uid counter,
//! and its three verbs are the only way anything enters it — [`Record::
//! replay`] for a line from history, [`Record::live`] for the daemon's
//! push, [`Record::seed`] for the one-time startup reconciliation. A
//! renderer's own UI state (scrolling, selection, a sidebar, key
//! bindings) stays out of it; `App` in app.rs holds one and delegates.

use std::collections::{HashMap, HashSet, VecDeque};

use cyclops_proto::{
    AgentState, Attention, AttentionItem, Clearance, DeliveryState, Event, Half, Kind, LedgerLine,
    NotifyLevel, OpenDelivery, PaneSnapshot, Resolved,
};
use serde_json::Value;

use crate::grid;

/// The one-shot startup reconciliation: which sessions the daemon watches,
/// where every pane stands right now, and every delivery it still counts
/// as needing a human.
///
/// This answer is the whole count. `--backfill` replays lines onto the
/// screen and feeds the register nothing, so no backfill value can change
/// what the eye says (`cyclops_proto::attention`).
#[derive(Debug, Clone, Default)]
pub struct StatusSeed {
    /// Sessions the daemon watches, in its own words. The backfill reads
    /// these ledgers and no others, so both halves agree on which sessions
    /// exist.
    pub watched: Vec<String>,
    /// Every pane the answer listed, in the register's own shape: the two
    /// names travel to `snapshot_agents` under their field names, so a
    /// transposition of label and pane id cannot compile.
    pub panes: Vec<PaneSnapshot>,
    pub open: Vec<OpenDelivery>,
    /// The same panes again, in the roster's richer shape. Separate from
    /// `panes` because the register's PaneSnapshot is the attention
    /// rule's input and grows for nobody else's convenience.
    pub roster: Vec<RosterSeed>,
}

/// One pane as a roster wants it seeded: everything a sidebar shows.
#[derive(Debug, Clone)]
pub struct RosterSeed {
    pub pane_id: String,
    pub name: String,
    pub state: AgentState,
    /// Which CLI the daemon detects in the pane, e.g. "claude".
    pub manifest: Option<String>,
    /// How long the pane had been in `state` when the answer was taken,
    /// by the daemon's clock. None from a daemon that predates the field.
    pub state_ms: Option<u64>,
}

impl StatusSeed {
    /// Normalize one `status` answer. The pane's display name resolves
    /// through `PaneStatus::display_name`, the same call `cyclops status`
    /// makes, so one pane never wears two names across surfaces.
    pub fn from_status(res: &cyclops_proto::StatusResult) -> StatusSeed {
        StatusSeed {
            watched: res.sessions.iter().map(|s| s.name.clone()).collect(),
            panes: res
                .sessions
                .iter()
                .flat_map(|s| &s.panes)
                .map(|p| PaneSnapshot {
                    pane_id: p.pane_id.clone(),
                    name: p.display_name().to_string(),
                    state: p.state,
                })
                .collect(),
            open: res.open_deliveries.clone(),
            roster: res
                .sessions
                .iter()
                .flat_map(|s| &s.panes)
                .map(|p| RosterSeed {
                    pane_id: p.pane_id.clone(),
                    name: p.display_name().to_string(),
                    state: p.state,
                    manifest: p.manifest.clone(),
                    state_ms: p.state_ms,
                })
                .collect(),
        }
    }
}

/// Ring capacity. The stream stays fluid past this because rendering is
/// windowed; older entries stay in the ledger, which is the record anyway.
pub const RING_CAP: usize = 10_000;

#[derive(Debug, Clone)]
pub struct Entry {
    /// Stable identity within one run, assigned by [`Record`] at push.
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
    /// beside the alarm row it answers, which stays exactly where it is
    /// (`docs/development/INVARIANTS.md` rule 8: the record appends, it does not
    /// retract).
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
            // this line's: `Record::admits` asks it before the calm view
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

/// Startup ordering: live entries buffer until the backfill lands, then
/// flush behind it, with ledger-backed duplicates dropped by seq. One
/// watched session dedupes exactly; with several, seq is ambiguous across
/// files and the rare startup-window duplicate is accepted (the ledger
/// itself never duplicates).
///
/// The status seed waits for the backfill too, and lands between the two
/// groups. All three carry a different age and the order is the whole
/// point: the replayed tail is history, the seed is the daemon's answer
/// about now, and a live entry that queued during startup is newer than
/// either. Applying the seed last let a fold taken before a transition
/// re-open an item that transition had just closed.
///
/// Pure buffering, no IO: a caller feeds it whatever its own transport
/// produced (a socket subscription for `cyclops watch`, a different one
/// for a workspace panel), and gets back the same three-group ordering
/// either way.
pub struct Intake {
    backfilled: bool,
    pending: Vec<Entry>,
    pending_status: Option<Box<StatusSeed>>,
    max_seq: Option<u64>,
}

impl Default for Intake {
    fn default() -> Self {
        Self::new()
    }
}

impl Intake {
    pub fn new() -> Intake {
        Intake {
            backfilled: false,
            pending: Vec::new(),
            pending_status: None,
            max_seq: None,
        }
    }

    /// True once the backfill has landed and live entries flow through.
    pub fn is_backfilled(&self) -> bool {
        self.backfilled
    }

    /// A live entry: ready to show now, or empty while buffering.
    pub fn entry(&mut self, e: Entry) -> Vec<Entry> {
        if !self.backfilled {
            self.pending.push(e);
            return Vec::new();
        }
        if self.dup(&e) {
            Vec::new()
        } else {
            vec![e]
        }
    }

    /// The status seed: ready to apply now, or held until the backfill
    /// lands so it reconciles over the replayed tail rather than under it.
    pub fn status(&mut self, seed: Box<StatusSeed>) -> Option<Box<StatusSeed>> {
        if self.backfilled {
            return Some(seed);
        }
        self.pending_status = Some(seed);
        None
    }

    /// The backfill arrived: the three groups, in the order they must be
    /// applied.
    pub fn backfill(&mut self, entries: Vec<Entry>, max_seq: Option<u64>) -> Backfilled {
        self.backfilled = true;
        self.max_seq = max_seq;
        let pending = std::mem::take(&mut self.pending);
        Backfilled {
            replayed: entries,
            seed: self.pending_status.take(),
            live: pending.into_iter().filter(|e| !self.dup(e)).collect(),
        }
    }

    fn dup(&self, e: &Entry) -> bool {
        matches!((e.seq, self.max_seq), (Some(s), Some(m)) if s <= m)
    }
}

/// What the startup window produced, oldest claim first. Apply in field
/// order: `replayed` is history and moves nothing but the screen, `seed`
/// is the daemon's snapshot and replaces the register, `live` are the
/// transitions that happened while the two were loading.
pub struct Backfilled {
    pub replayed: Vec<Entry>,
    pub seed: Option<Box<StatusSeed>>,
    pub live: Vec<Entry>,
}

/// The ring, the attention register, and the uid counter: everything a
/// renderer needs to turn protocol events and ledger lines into ordered,
/// identity-stable rows, and nothing about how to paint them.
///
/// Three verbs are the only way anything enters it, one per source, which
/// is what lets [`Record::admits`] answer for a line without asking what
/// produced it: [`Record::replay`] for history, [`Record::live`] for the
/// daemon's push, [`Record::seed`] for the one-time startup reconciliation.
/// What needs a human is NOT decided anywhere else: the register and the
/// rule live in `cyclops_proto::attention`, and this only ever feeds it
/// the daemon's snapshot and live events, never a replayed line
/// (`cyclops_proto::attention`, "what may feed the register").
pub struct Record {
    entries: VecDeque<Entry>,
    next_uid: u64,
    attention: Attention,
    /// The newest state ingested per pane, keyed the way
    /// [`cyclops_proto::attention::observe_agent`] keys the register
    /// (`cyclops_proto::agent_key`). What `ingest` dedupes a State line
    /// against: the zombie-watcher bug this guards against re-emits the
    /// surviving watcher's own last reading once a second under a fresh
    /// `prior=None`, and nothing about the daemon's wire shape tells a
    /// repeat apart from a real transition except that the two are
    /// identical.
    last_agent_state: HashMap<String, AgentState>,
}

impl Default for Record {
    fn default() -> Self {
        Self::new()
    }
}

impl Record {
    pub fn new() -> Record {
        Record {
            entries: VecDeque::new(),
            next_uid: 1,
            attention: Attention::default(),
            last_agent_state: HashMap::new(),
        }
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The register itself, for surfaces that need more than the count.
    pub fn attention(&self) -> &Attention {
        &self.attention
    }

    pub fn attention_count(&self) -> usize {
        self.attention.count()
    }

    /// Every attention item as one phrase, in the stream's own voice
    /// ("reviewer  ⚠ blocked_permission", "reviewer  ⊘ parked · quota"),
    /// name-sorted by the register so the same backlog always reads the
    /// same way.
    ///
    /// Read wherever a count has to explain itself: the plain follow's eye
    /// line, which has no header to point at. Uncolored, because the eye
    /// line is the screen-reader path and never carries paint.
    pub fn attention_items(&self) -> Vec<String> {
        self.attention
            .items()
            .iter()
            .map(|i| grid::attention_phrase(i, &grid::Plain))
            .collect()
    }

    /// One line replayed from the record: it goes on the screen and
    /// nowhere else.
    ///
    /// History cannot answer "what needs a human right now". Letting it
    /// try is exactly what made `--backfill` decide the count: the same
    /// rig read at 200 lines and at 400 gave a closed eye and an open one.
    /// The daemon's snapshot and the live push own the register.
    pub fn replay(&mut self, e: Entry) {
        self.ingest(e);
    }

    /// One live event from the daemon: it goes on the record AND moves the
    /// register, because the event is the pane's or the delivery's own
    /// next transition.
    ///
    /// Returns the clearance line when the transition ended something that
    /// needed a human (`cyclops_proto::attention`, rule 3). It is already
    /// on the record; the caller gets it because a line-at-a-time renderer
    /// (`--plain`) has no frame to reconcile, so a line it never sees is a
    /// line it never prints.
    pub fn live(&mut self, e: Entry) -> Option<Entry> {
        let resolved = match &e.kind {
            EntryKind::State {
                target,
                pane_id,
                state,
            } => self
                .attention
                .observe_agent(target, pane_id.as_deref(), *state),
            EntryKind::Delivery { to, state, .. } => {
                self.attention.observe_delivery(to, e.id.as_deref(), *state)
            }
            // A pane leaving the table is its last transition, and the
            // only thing that can drop its attention item while the
            // process runs: no state event ever arrives for a pane that
            // is gone.
            EntryKind::PaneGone { pane_id } => self.attention.forget_agent(pane_id),
            _ => None,
        };
        let (ts, id) = (e.ts, e.id.clone());
        self.ingest(e);
        // Nothing ended: the transition is on the record and that is all
        // there is to say about it.
        let resolved = resolved?;
        self.ingest(Entry::cleared(ts, id, resolved));
        // Copied back off the ring so the caller's entry carries the uid
        // the ring gave it, not a placeholder.
        self.entries.back().cloned()
    }

    /// Assign a uid and ring the entry. Uids are never reused within a
    /// run, which is what lets a caller anchor a selection or a scroll
    /// position by uid across an update instead of an index into a ring
    /// that evicts.
    ///
    /// A State line that says exactly what `last_agent_state` already holds
    /// for that pane is dropped before either happens: a duplicate must
    /// never consume a uid, and it must never occupy a ring slot that a
    /// real transition or a firehose reader would otherwise get. PaneGone
    /// clears the pane's entry so a pane that comes back under the same id
    /// is judged fresh rather than against a reading from its previous
    /// life.
    fn ingest(&mut self, mut e: Entry) {
        match &e.kind {
            EntryKind::State {
                target,
                pane_id,
                state,
            } => {
                let key = cyclops_proto::agent_key(target, pane_id.as_deref()).to_string();
                if self.last_agent_state.get(&key) == Some(state) {
                    return;
                }
                self.last_agent_state.insert(key, *state);
            }
            EntryKind::PaneGone { pane_id } => {
                self.last_agent_state.remove(pane_id);
            }
            _ => {}
        }
        e.uid = self.next_uid;
        self.next_uid += 1;
        if self.entries.len() == RING_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(e);
    }

    /// Startup reconciliation from the daemon's one status answer: the
    /// register replaced whole, then a line for everything it counts that
    /// the loaded record does not already carry, then the mirror image —
    /// a clearance for everything the loaded record still shows as an
    /// open alarm that the answer no longer counts.
    ///
    /// The answer REPLACES the register. It is a snapshot of now, folded
    /// from the whole record on the delivery side and read off the live
    /// pane table on the agent side, so anything it does not list is
    /// resolved or gone. Merging into it left a blocked pane that later
    /// disappeared counted for the life of the process, with no event able
    /// to clear it.
    ///
    /// Returns the entries the caller should ingest (via [`Record::
    /// replay`]), in two directions. One line for each seeded item the
    /// loaded record does not already account for, so no number a header
    /// shows is left without a line behind it; and one clearance for each
    /// alarm the loaded record DOES show that the answer no longer counts,
    /// so no line saying a human is needed is left without the news that
    /// it ended.
    ///
    /// Event-driven only: called once at startup and never on a timer.
    /// After that the register moves on live events alone, and a pane
    /// leaving the table is one of them ([`Record::live`]).
    pub fn seed(&mut self, panes: &[PaneSnapshot], open: &[OpenDelivery]) -> Vec<Entry> {
        // 1. Replace both halves of the register with the answer.
        self.attention.snapshot_agents(panes.iter().cloned());
        self.attention.snapshot_deliveries(open);
        // 2. Write a line for every counted item the loaded record does
        //    not already carry. The register says WHICH items those are,
        //    so a header and the record cannot disagree about the list;
        //    the answer supplies when each one happened.
        //
        //    Both lookups are indexed once, not searched per item: the
        //    backlog is the item count and a quota weekend leaves hundreds
        //    of parked deliveries, so a scan per item costs items squared.
        let items = self.attention.items();
        let newest = self.newest_line_per_item(&items);
        let open_by_key: HashMap<(&str, &str), &OpenDelivery> = open
            .iter()
            .map(|d| ((d.to.as_str(), d.id.as_str()), d))
            .collect();
        let mut out = Vec::new();
        for item in &items {
            if newest
                .get(&item.identity())
                .is_some_and(|e| says_the_same(e, item))
            {
                continue; // the record already says this
            }
            out.push(match item {
                AttentionItem::Agent {
                    pane_id,
                    name,
                    state,
                } => Entry {
                    uid: 0,
                    // No transition time travels with a status answer, so
                    // the line is stamped when the reading was taken. It
                    // says where the pane stands now, which is what status
                    // is.
                    ts: crate::data::now_ms(),
                    seq: None,
                    id: None,
                    kind: EntryKind::State {
                        target: name.clone(),
                        pane_id: Some(pane_id.clone()),
                        state: *state,
                    },
                },
                AttentionItem::Delivery { to, id, state } => {
                    let record = open_by_key.get(&(to.as_str(), id.as_str())).copied();
                    Entry {
                        uid: 0,
                        // The record's own transition time: this line can
                        // be hours older than the replayed tail above it,
                        // and saying so is the point of showing it at all.
                        ts: record.map_or_else(crate::data::now_ms, |d| d.ts),
                        seq: None,
                        id: Some(id.clone()),
                        kind: EntryKind::Delivery {
                            to: to.clone(),
                            state: *state,
                            cause: record.and_then(|d| d.cause.clone()),
                        },
                    }
                }
            });
        }
        // 3. And the mirror of step 2. The loaded record can hold an alarm
        //    whose item the answer does not count: a park requeued while
        //    the UI was down, a pane that unblocked or went away. Its line
        //    is on screen saying a human is needed, and the transition
        //    that ended it is either older than the tail or was never a
        //    line the calm view takes. The register says which alarms
        //    those are and how each one ended; the clearance is what puts
        //    that under the row the reader is looking at.
        for (item, how) in self.alarms_the_answer_cleared() {
            out.push(Entry::cleared(
                crate::data::now_ms(),
                None,
                Resolved { was: item, how },
            ));
        }
        // 4. The dedup map has to move with the register it guards, for
        // every pane `out` did not just write a fresh State line for. A
        // pane whose alarm turned into the clearance just pushed above
        // leaves no State line behind (a clearance is not one), so without
        // this the map would keep saying "blocked" after the register
        // itself says otherwise, and the pane's next real return to that
        // same blocked state would be dropped as a duplicate of a reading
        // the register no longer holds. Panes step 2 DID write a line for
        // are excluded here on purpose: that line sets the map itself the
        // moment the caller replays it, and setting it here first would
        // make `ingest` mistake that very line for the duplicate it is not.
        let just_written: HashSet<&str> = out
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::State { pane_id, .. } => pane_id.as_deref(),
                _ => None,
            })
            .collect();
        for pane in panes {
            if !just_written.contains(pane.pane_id.as_str()) {
                self.last_agent_state
                    .insert(pane.pane_id.clone(), pane.state);
            }
        }
        out
    }

    /// Every alarm the loaded record still shows with nothing under it
    /// that answers, paired with the register's account of how it ended.
    ///
    /// One walk of the ring, oldest first, holding the newest alarm per
    /// item: a later alarm about the same item supersedes an earlier one
    /// (one pane has one current state), and a clearance already on the
    /// record retires it. Sorted by name so a startup over a long tail
    /// always writes them in the same order.
    fn alarms_the_answer_cleared(&self) -> Vec<(AttentionItem, Clearance)> {
        // Owned keys: the map outlives each entry's borrow, and the
        // identity is two strings either way.
        let key = |item: &AttentionItem| {
            let (half, name, id) = item.identity();
            (half, name.to_string(), id.to_string())
        };
        let mut open: HashMap<(Half, String, String), AttentionItem> = HashMap::new();
        for e in &self.entries {
            match (alarm_item(e), &e.kind) {
                (Some(item), _) => {
                    open.insert(key(&item), item);
                }
                (None, EntryKind::Cleared { was, .. }) => {
                    open.remove(&key(was));
                }
                _ => {}
            }
        }
        let mut out: Vec<(AttentionItem, Clearance)> = open
            .into_values()
            .filter_map(|item| {
                let how = self.attention.clearance(item.identity())?;
                Some((item, how))
            })
            .collect();
        out.sort_by(|(a, _), (b, _)| (a.name(), a.identity()).cmp(&(b.name(), b.identity())));
        out
    }

    /// The newest loaded line for each of `items`, from ONE walk of the
    /// ring rather than one walk per item.
    ///
    /// The backlog a human has to clear is the item count, and it is a
    /// backlog: hundreds of parked deliveries after a quota weekend is an
    /// ordinary reading, against a ring that holds ten thousand lines.
    fn newest_line_per_item<'a>(
        &'a self,
        items: &'a [AttentionItem],
    ) -> HashMap<(Half, &'a str, &'a str), &'a Entry> {
        let wanted: HashSet<(Half, &str, &str)> = items.iter().map(|i| i.identity()).collect();
        let mut newest = HashMap::with_capacity(items.len());
        for e in self.entries.iter().rev() {
            if newest.len() == wanted.len() {
                break;
            }
            let Some(id) = entry_identity(e) else {
                continue;
            };
            if wanted.contains(&id) {
                newest.entry(id).or_insert(e);
            }
        }
        newest
    }

    /// Does the calm view admit this line?
    ///
    /// Every kind but one answers for itself, by the rule and nothing else
    /// ([`Entry::admin_visible`]). The daemon's admin pings are the
    /// exception, and they need the register rather than the line: a ping
    /// POINTS AT something that needs a human, it is not itself a state,
    /// so no transition can ever clear it and it cannot join the register
    /// either. A ping admitted regardless kept saying "action required"
    /// after its delivery moved on, and pinged about conditions the rule
    /// says nobody must clear (a wedged gate hold, a downgraded hook
    /// verification), which is how "⚠ action required" came to render
    /// directly under a closed eye.
    ///
    /// So a ping that claims a human is needed is admitted only while the
    /// register still holds an item it names. One ping may name several
    /// (the restart closure ends a whole run's worth of deliveries at
    /// once), and it still stands while ANY of them does: the others have
    /// been dealt with, this one has not. A ping that names no item (an
    /// operator's own `admin.notify`, a daemon that predates the naming)
    /// is admitted: nothing here can prove it stale, and dropping a
    /// human's own ping is the worse failure.
    pub fn admits(&self, e: &Entry) -> bool {
        if !e.admin_visible() {
            return false;
        }
        let mut items = ping_items(e).peekable();
        if items.peek().is_none() {
            return true; // names nothing the register could answer for
        }
        items.any(|item| self.attention.holds(item))
    }

    /// Counted items with no line in `visible`.
    ///
    /// A header may never show a number the reader cannot reach, so
    /// whatever this returns has to be said on the frame. Two things put
    /// an item here: a filter that hides its line, and eviction from the
    /// ring. The startup reconciliation writes a line for everything else,
    /// so nothing else can.
    ///
    /// One walk of `visible`, not one per item: the caller passes it in
    /// because a renderer already walked its own filtered view once to
    /// build the frame, and a second scan per item makes that walk cost
    /// items x window.
    pub fn unreachable(&self, visible: &[&Entry]) -> Vec<AttentionItem> {
        let items = self.attention.items();
        if items.is_empty() {
            return Vec::new();
        }
        let mut reached = vec![false; items.len()];
        {
            // The register keys are unique, so identity indexes the
            // backlog exactly and the window is walked once against it.
            let by_id: HashMap<(Half, &str, &str), usize> = items
                .iter()
                .enumerate()
                .map(|(i, item)| (item.identity(), i))
                .collect();
            for e in visible {
                let Some(id) = entry_identity(e) else {
                    continue;
                };
                if let Some(&i) = by_id.get(&id) {
                    reached[i] |= says_the_same(e, &items[i]);
                }
            }
        }
        items
            .into_iter()
            .zip(reached)
            .filter(|(_, reached)| !reached)
            .map(|(item, _)| item)
            .collect()
    }
}

/// The item a line could be about, by name alone. Lines that name nothing
/// the register tracks (messages, gates, session churn) answer None.
///
/// The identity is the register's own ([`AttentionItem::identity`]): the
/// pane key across adoption, or (recipient, message id). Keeping it a
/// value rather than a comparison is what lets a surface index its lines
/// once instead of rescanning them per item.
fn entry_identity(e: &Entry) -> Option<(Half, &str, &str)> {
    match &e.kind {
        EntryKind::State {
            target, pane_id, ..
        } => Some((
            Half::Agent,
            cyclops_proto::agent_key(target, pane_id.as_deref()),
            "",
        )),
        EntryKind::Delivery { to, .. } => {
            Some((Half::Delivery, to, e.id.as_deref().unwrap_or_default()))
        }
        // A clearance is about its item, and carries the identity itself
        // rather than deriving it from a name and a record id.
        EntryKind::Cleared { was, .. } => Some(was.identity()),
        _ => None,
    }
}

/// The item this line raises, when the line says a human is needed.
///
/// The judgement is `Entry::admin_visible`'s, asked of the same rule: what
/// makes a line an alarm is what puts it in the calm view. Gate holds are
/// not here on purpose. A hold says a human is needed because a PANE is
/// blocked, so it has no item of its own and the pane's clearance is the
/// one that answers it.
fn alarm_item(e: &Entry) -> Option<AttentionItem> {
    match &e.kind {
        EntryKind::State {
            target,
            pane_id,
            state,
        } if state.is_blocked() => Some(AttentionItem::Agent {
            pane_id: cyclops_proto::agent_key(target, pane_id.as_deref()).to_string(),
            name: target.clone(),
            state: *state,
        }),
        EntryKind::Delivery { to, state, .. } if cyclops_proto::delivery_needs_human(*state) => {
            Some(AttentionItem::Delivery {
                to: to.clone(),
                id: e.id.clone().unwrap_or_default(),
                state: *state,
            })
        }
        _ => None,
    }
}

/// Every register item a PING claims a human is needed for, in the
/// identity the register keys on. Empty means the ping names nothing.
///
/// `fyi` pings claim nothing, so nothing about them can contradict a calm
/// eye and they are never held to the register. The claim lives in how the
/// level renders ("⚠ action required", "⚠ urgent" against a dim "fyi",
/// entry.rs `content`), which is why the level is read here and the rule
/// is not restated.
///
/// An iterator rather than a Vec: this runs for every ping on every frame
/// (`Record::admits`), and a firehose over a full ring is where the frame
/// budget goes.
fn ping_items(e: &Entry) -> impl Iterator<Item = (Half, &str, &str)> {
    // The single-item form every ping about one thing uses, and the batch
    // list the restart closure adds. A ping carries one or the other.
    let (one, batch): (Option<(Half, &str, &str)>, &[PingDelivery]) = match &e.kind {
        EntryKind::Notify {
            level,
            pane_id,
            to,
            deliveries,
            ..
        } if !matches!(level, NotifyLevel::Fyi) => {
            let one = match (pane_id, to) {
                (Some(pane_id), _) => Some((Half::Agent, pane_id.as_str(), "")),
                (None, Some(to)) => Some((
                    Half::Delivery,
                    to.as_str(),
                    e.id.as_deref().unwrap_or_default(),
                )),
                (None, None) => None,
            };
            (one, deliveries.as_slice())
        }
        _ => (None, &[]),
    };
    one.into_iter().chain(
        batch
            .iter()
            .map(|d| (Half::Delivery, d.to.as_str(), d.id.as_str())),
    )
}

/// Does this line say what the register currently claims about that item?
///
/// The second half of "is it evidence"; identity is the caller's to match.
/// An older line for the same pane is not evidence: it says something the
/// register no longer claims, and pointing a reader at it would be worse
/// than saying nothing.
fn says_the_same(e: &Entry, item: &AttentionItem) -> bool {
    match (&e.kind, item) {
        (EntryKind::State { state, .. }, AttentionItem::Agent { state: want, .. }) => state == want,
        (EntryKind::Delivery { state, .. }, AttentionItem::Delivery { state: want, .. }) => {
            state == want
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::{AgentState, DeliveryState};
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
            &ev("state", json!({"target": "reviewer", "state": "working"})),
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

    fn entry(ts: u64, seq: Option<u64>) -> Entry {
        Entry {
            uid: 0,
            ts,
            seq,
            id: None,
            kind: EntryKind::Other {
                event: "x".into(),
                detail: None,
            },
        }
    }

    #[test]
    fn intake_buffers_until_backfill_then_dedupes_by_seq() {
        let mut i = Intake::new();
        // Live entries before the backfill wait.
        assert!(i.entry(entry(5, Some(3))).is_empty());
        assert!(i.entry(entry(6, Some(4))).is_empty());
        assert!(i.entry(entry(7, None)).is_empty());
        // Backfill covers seq 1..=3: the seq-3 pending entry is a dupe,
        // seq 4 and the seq-less one flush behind the backfill.
        let landed = i.backfill(vec![entry(1, Some(1)), entry(3, Some(3))], Some(3));
        assert!(landed.seed.is_none(), "no status seed was waiting");
        let replayed: Vec<Option<u64>> = landed.replayed.iter().map(|e| e.seq).collect();
        assert_eq!(replayed, vec![Some(1), Some(3)]);
        let live: Vec<Option<u64>> = landed.live.iter().map(|e| e.seq).collect();
        assert_eq!(live, vec![Some(4), None]);
        // After the merge, stale live copies still drop; fresh ones pass.
        assert!(i.entry(entry(8, Some(2))).is_empty());
        assert_eq!(i.entry(entry(9, Some(5))).len(), 1);
    }

    #[test]
    fn intake_without_a_cursor_keeps_everything() {
        let mut i = Intake::new();
        assert!(i.entry(entry(5, Some(3))).is_empty());
        let landed = i.backfill(vec![entry(1, Some(9))], None);
        assert_eq!(landed.replayed.len(), 1);
        assert_eq!(landed.live.len(), 1, "no cursor means no dedupe");
    }

    /// The seed lands between the replayed tail and the live entries that
    /// queued behind it. Under the tail, a ledger line older by
    /// construction would overwrite the daemon's answer about now; over
    /// the live entries, a fold taken before a transition would re-open
    /// the item that transition just closed.
    #[test]
    fn the_status_seed_lands_between_history_and_the_live_backlog() {
        let mut i = Intake::new();
        assert!(i.entry(entry(9, None)).is_empty());
        let seed = Box::new(StatusSeed::default());
        assert!(i.status(seed).is_none(), "the seed jumped the backfill");
        let landed = i.backfill(vec![entry(1, None)], None);
        assert_eq!(landed.replayed.len(), 1, "history first");
        assert!(landed.seed.is_some(), "the seed never came back");
        assert_eq!(landed.live.len(), 1, "the live backlog goes last");
        // Once the backfill has landed, a late seed applies straight away.
        assert!(i.status(Box::new(StatusSeed::default())).is_some());
    }

    #[test]
    fn ring_caps_and_uids_stay_unique() {
        let mut r = Record::new();
        for _ in 0..(RING_CAP + 10) {
            r.live(msg("codex", &["reviewer"], "s"));
        }
        assert_eq!(r.len(), RING_CAP);
        // The oldest were evicted: the first uid in the ring is 11.
        assert_eq!(r.entries().next().unwrap().uid, 11);
    }

    /// The seam a future workspace panel's parity test reuses: one
    /// backfill-plus-live transcript, fed through the exact ordering
    /// [`Intake`] enforces, has to yield the four guarantees any renderer
    /// depends on: row order, stable identity across the update, the
    /// resolution row an ending alarm gets, and a calm-view decision that
    /// answers to the register's CURRENT state rather than to a stale scan.
    #[test]
    fn a_backfill_plus_live_transcript_yields_ordered_identity_stable_rows() {
        let mut intake = Intake::new();
        let mut record = Record::new();

        // The live push starts arriving before the backfill answers, the
        // ordinary startup race.
        assert!(intake
            .entry(state_entry(5_000, "reviewer", "%1", AgentState::Idle))
            .is_empty());

        // The daemon's own answer says reviewer is blocked right now; it
        // buffers too, ordered between the replayed tail and the live
        // backlog.
        let status = StatusSeed {
            watched: vec!["main".into()],
            panes: vec![cyclops_proto::PaneSnapshot {
                pane_id: "%1".into(),
                name: "reviewer".into(),
                state: AgentState::BlockedPermission,
            }],
            open: Vec::new(),
            roster: Vec::new(),
        };
        assert!(intake.status(Box::new(status)).is_none());

        // The backfill lands: one replayed line, older than everything
        // above it.
        let landed = intake.backfill(
            vec![state_entry(1_000, "reviewer", "%1", AgentState::Working)],
            None,
        );

        for e in landed.replayed {
            record.replay(e);
        }
        let seed = landed.seed.expect("the seed came back");
        for e in record.seed(&seed.panes, &seed.open) {
            record.replay(e);
        }

        // The calm-view filter decision, mid-transcript: an admin ping
        // about %1 stands on the seed's alarm before the live transition
        // below ever runs. `admits` takes any entry, ring or not, so the
        // same ping object probes the register at two points in time.
        let ping = ping_entry(4_000, "p-1", "%1");
        assert!(
            record.admits(&ping),
            "the ping should stand on the seed's alarm"
        );

        let mut cleared_rows = Vec::new();
        for e in landed.live {
            if let Some(c) = record.live(e) {
                cleared_rows.push(c);
            }
        }

        // The same ping, asked again after the alarm resolved: `admits`
        // reads the register as it stands now, not a stale snapshot, so
        // the answer flips once the ping's own item is gone.
        assert!(
            !record.admits(&ping),
            "the ping outlived the alarm it was about"
        );

        // Row order: the replayed tail leads, the seed's own
        // reconciliation line lands after it (stamped "now", newer than
        // anything replayed), and the live backlog — the idle transition
        // plus the resolution row it produced — trails both.
        let rows: Vec<&Entry> = record.entries().collect();
        let ts: Vec<u64> = rows.iter().map(|e| e.ts).collect();
        assert_eq!(rows.len(), 4, "{ts:?}");
        assert_eq!(ts[0], 1_000, "the replayed tail must lead: {ts:?}");
        assert!(
            ts[1] > ts[0],
            "the seed line must land after the tail: {ts:?}"
        );
        assert_eq!(
            &ts[2..],
            &[5_000, 5_000],
            "the live backlog and its resolution row must trail: {ts:?}"
        );

        // Stable identity: uids are assigned once, in row order, and never
        // reused. An incremental consumer (a Ratatui list keyed by uid,
        // say) can diff against them across an update instead of an index
        // into a ring that evicts.
        let uids: Vec<u64> = rows.iter().map(|e| e.uid).collect();
        assert_eq!(uids, vec![1, 2, 3, 4], "uids must be assigned in row order");

        // Resolution-row pairing: the live idle transition resolved the
        // seed's blocked-permission alarm, and the row it produced is
        // already the last row on the record, naming exactly that alarm
        // (INVARIANTS rule 8: the record appends, it does not retract).
        assert_eq!(cleared_rows.len(), 1);
        assert_eq!(cleared_rows[0].uid, rows[3].uid);
        match &cleared_rows[0].kind {
            EntryKind::Cleared { was, how } => {
                assert!(matches!(was, AttentionItem::Agent { name, .. } if name == "reviewer"));
                assert_eq!(*how, Clearance::Moved);
            }
            other => panic!("expected a Cleared row, got {other:?}"),
        }
        assert_eq!(record.attention_count(), 0, "the live idle cleared it");
    }

    /// The zombie-watcher bug this guards against: a dead watcher's stale
    /// recompute re-emits the same reading a live watcher already reported,
    /// once a second, forever. Replayed (a ledger written by that bug) or
    /// live (the daemon still running it), the second copy must not land.
    #[test]
    fn replaying_two_identical_blocked_lines_admits_exactly_one() {
        let mut r = Record::new();
        r.replay(state_entry(
            1_000,
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        ));
        r.replay(state_entry(
            2_000,
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        ));
        assert_eq!(r.len(), 1, "the duplicate must not occupy a ring slot");
        // Nor a uid: the next distinct line still gets uid 2.
        r.replay(state_entry(3_000, "reviewer", "%1", AgentState::Idle));
        let uids: Vec<u64> = r.entries().map(|e| e.uid).collect();
        assert_eq!(
            uids,
            vec![1, 2],
            "the dropped duplicate must not consume a uid"
        );
    }

    #[test]
    fn living_two_identical_blocked_lines_admits_exactly_one() {
        let mut r = Record::new();
        r.live(state_entry(
            1_000,
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        ));
        r.live(state_entry(
            2_000,
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        ));
        assert_eq!(r.len(), 1);
        assert_eq!(
            r.attention_count(),
            1,
            "the register still counts the one pane, not two"
        );
    }

    /// The dedup answers by state, not by pane: a real transition and back
    /// again is two rows a reader must see, with the clearance that ended
    /// the first one sitting between them.
    #[test]
    fn a_real_blocked_working_blocked_cycle_admits_both_blocked_rows() {
        let mut r = Record::new();
        r.live(state_entry(
            1_000,
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        ));
        let cleared = r.live(state_entry(2_000, "reviewer", "%1", AgentState::Working));
        assert!(
            cleared.is_some(),
            "working must end the first blocked alarm"
        );
        r.live(state_entry(
            3_000,
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        ));

        let rows: Vec<&Entry> = r.entries().collect();
        assert_eq!(
            rows.len(),
            4,
            "both blocked rows and the clearance between them must all land"
        );
        assert!(matches!(&rows[0].kind, EntryKind::State { state, .. } if state.is_blocked()));
        assert!(matches!(&rows[1].kind, EntryKind::State { state, .. } if !state.is_blocked()));
        assert!(matches!(&rows[2].kind, EntryKind::Cleared { .. }));
        assert!(matches!(&rows[3].kind, EntryKind::State { state, .. } if state.is_blocked()));
    }

    /// The scenario the seed overwrite exists for: a replayed tail ends
    /// on a blocked reading, the daemon's answer says the pane moved on,
    /// and the record only gets a clearance line for that (never a State
    /// line, since an unblocked pane is not an attention item). Without
    /// the seed also syncing the dedup map, the map would still say
    /// blocked, and the pane's next real return to it would be silently
    /// dropped as a duplicate of a reading the register no longer holds.
    #[test]
    fn seed_overwrites_the_dedup_map_so_the_next_real_blocked_line_lands() {
        let mut r = Record::new();
        r.replay(state_entry(
            1_000,
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        ));
        let seeded = r.seed(
            &[PaneSnapshot {
                pane_id: "%1".into(),
                name: "reviewer".into(),
                state: AgentState::Idle,
            }],
            &[],
        );
        for e in seeded {
            r.replay(e);
        }
        let before = r.len();
        r.live(state_entry(
            2_000,
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        ));
        assert_eq!(
            r.len(),
            before + 1,
            "the seed must not leave the map saying blocked forever"
        );
        assert_eq!(r.attention_count(), 1);
    }

    /// A pane that leaves the table and comes back under the same id (a
    /// respawn, a session detach/reattach cycle) is judged fresh: its old
    /// reading must not stand in for its new one.
    #[test]
    fn pane_gone_clears_the_dedup_key_so_a_recreated_pane_is_judged_fresh() {
        let mut r = Record::new();
        r.live(state_entry(
            1_000,
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        ));
        r.live(Entry {
            uid: 0,
            ts: 2_000,
            seq: None,
            id: None,
            kind: EntryKind::PaneGone {
                pane_id: "%1".into(),
            },
        });
        r.live(state_entry(
            3_000,
            "reviewer",
            "%1",
            AgentState::BlockedPermission,
        ));
        let blocked_rows = r
            .entries()
            .filter(|e| matches!(&e.kind, EntryKind::State { state, .. } if state.is_blocked()))
            .count();
        assert_eq!(
            blocked_rows, 2,
            "the pane's return under the same id must not be judged a duplicate"
        );
    }

    fn state_entry(ts: u64, target: &str, pane_id: &str, state: AgentState) -> Entry {
        Entry {
            uid: 0,
            ts,
            seq: None,
            id: None,
            kind: EntryKind::State {
                target: target.into(),
                pane_id: Some(pane_id.into()),
                state,
            },
        }
    }

    fn ping_entry(ts: u64, id: &str, pane_id: &str) -> Entry {
        Entry {
            uid: 0,
            ts,
            seq: None,
            id: Some(id.into()),
            kind: EntryKind::Notify {
                level: NotifyLevel::ActionRequired,
                subject: "a prompt is waiting".into(),
                pane_id: Some(pane_id.into()),
                to: None,
                deliveries: Vec::new(),
            },
        }
    }
}
