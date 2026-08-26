//! What needs a human right now.
//!
//! One vocabulary, one owner. The eye is the signature device
//! (GOALS), and it appears on three surfaces: the stream header, the
//! `--plain` eye line, and `cyclops status`. All three read this module.
//! None of them reimplements the predicates. Every one of them counts three
//! halves: live agent state, legacy session-ledger delivery alarms, and
//! durable mailbox attention. `cyclops status` asks for open deliveries and
//! builds [`Attention::from_status`]; [`Attention::from_live_status`] is the
//! pane half alone, for an answer that carried no open deliveries.
//!
//! ## The rule
//!
//! 1. An AGENT needs a human while its fused state is blocked
//!    ([`AgentState::is_blocked`]): a permission prompt, a vendor modal,
//!    or an exhausted quota. Nothing downstream clears any of the three.
//!    A surface that only holds the gate's WORD for that state asks
//!    [`gate_cause_needs_human`], which maps the word back onto the state.
//! 2. A DELIVERY needs a human while its normalized row is
//!    `attention_required` or `parked_blocked_quota`
//!    ([`delivery_needs_human`]). Those rows include legacy delivery alarms,
//!    open mailbox attention attempts, pre-write-blocked heads, and quota
//!    holds. Recovery follows the row's cause: a pre-write block may reopen
//!    when its evidence changes or the recipient claims it, while exhausted
//!    redelivery and quota states require their named operator action.
//! 3. When an item STOPS needing a human, the transition that ended it is
//!    part of the same story, so the register hands it back ([`Resolved`])
//!    instead of dropping the item in silence. The live mutators return
//!    one; a surface reconciling lines it ALREADY shows against a fresh
//!    snapshot asks [`Attention::clearance`], which is the same question
//!    put to the roster. Either way the surface that showed the alarm is
//!    the one that gets to show it end.
//!
//!    The alarm reaches the calm view because it says a human is needed;
//!    the transition that resolves it is ordinary traffic and does not, so
//!    the calm view kept the alarm and never showed it end, and a closed
//!    eye sat over a row saying a human was needed. The stream is a record
//!    and records do not retract, so the alarm line stays where it is. The
//!    fix is the second line: rule 3 is what produces it, in one place,
//!    for both halves.
//!
//! ## What may feed the register
//!
//! This is as much of the rule as the two predicates are, because it
//! decides what a count is allowed to depend on.
//!
//! 1. A stream asks the daemon for a `status` snapshot containing the pane
//!    roster, the legacy open deliveries folded from the session record, and
//!    the durable mailbox attention rows. It REPLACES all three halves
//!    ([`Attention::snapshot_agents`], [`Attention::snapshot_deliveries`],
//!    [`Attention::snapshot_mailbox`]), so a pane that is gone stops
//!    counting and an item nothing could clear cannot outlive the answer.
//!    After that the mailbox half is replaced again by every
//!    `messages.snapshot` the stream's refresh gate accepts, stamped by the
//!    same `workspace_seq` as its Messages view. `cyclops status` reads the
//!    same answer once and prints it.
//! 2. Live events move one item at a time ([`Attention::observe_agent`],
//!    [`Attention::observe_delivery`], [`Attention::forget_agent`]),
//!    because each one IS the pane's or the delivery's next transition.
//!    A pane leaving the tmux table is its LAST transition: no event can
//!    ever arrive for a pane that is gone, so without that edge a blocked
//!    item outlives the pane for the life of the reading process. The
//!    snapshot is taken once, at startup, and nothing re-takes it on a
//!    timer (zero polling), so the edge is the only thing that can drop
//!    the item while the process runs.
//! 3. Replayed history feeds it NOTHING. A window over the record cannot
//!    answer "right now", and letting it try means the size of that window
//!    (`cyclops ui --backfill N`) decides the count. That is the bug this
//!    module exists to make impossible; the ordering above is the fix.
//!
//! Order matters when all three arrive at startup: replayed tail first
//! (screen only), then the snapshot, then the live entries that queued
//! behind them. Newest wins, which is why the snapshot may not be applied
//! last.

use std::collections::BTreeMap;

use serde::de::value::{Error as ValueError, StrDeserializer};
use serde::de::IntoDeserializer;
use serde::Deserialize;

use crate::identity::RecipientKey;
use crate::ledger::DeliveryState;
use crate::state::AgentState;
use crate::wire::{OpenDelivery, PaneStatus, StatusResult};

/// The rule's delivery half: states the pipeline cannot leave on its own.
///
/// Read by the stream's calm view, by the eye's count, and by the daemon's
/// fold behind `status`'s open deliveries. One definition, so no two
/// surfaces can disagree about what an open delivery is.
pub fn delivery_needs_human(state: DeliveryState) -> bool {
    matches!(
        state,
        DeliveryState::AttentionRequired | DeliveryState::ParkedBlockedQuota
    )
}

const DELIVERY_PRE_WRITE_CAUSE_PREFIX: &str = "blocked_pre_write:";

/// Encode the reason carried by a normalized pre-write mailbox row.
///
/// The row uses the legacy `OpenDelivery.cause` string, so producers and
/// consumers share this protocol-owned encoding instead of matching a prefix
/// independently.
pub fn delivery_pre_write_cause(reason: &str) -> String {
    format!("{DELIVERY_PRE_WRITE_CAUSE_PREFIX}{reason}")
}

/// Decode the reason carried by a normalized pre-write mailbox row.
pub fn delivery_pre_write_reason(cause: &str) -> Option<&str> {
    cause
        .strip_prefix(DELIVERY_PRE_WRITE_CAUSE_PREFIX)
        .filter(|reason| !reason.is_empty())
}

/// The rule's agent half as the delivery gate spells it.
///
/// A gate line records WHY a delivery is held as a string. The daemon
/// writes `blocked:<rule id>` when a trust or permission prompt owns the
/// screen (cyclopsd/src/delivery.rs), and otherwise the fused state's own
/// name: `working`, `blocked_quota`, `idle_with_input`. Only the blocked
/// ones are a human's to clear, and which states those are is rule 1
/// above, not the spelling of the word.
///
/// Reading the prefix in the stream was a third copy of the rule that
/// nothing tied to the other two: it answered yes for any future cause
/// that happened to begin with "blocked".
pub fn gate_cause_needs_human(cause: &str) -> bool {
    // "blocked:<rule id>" names the prompt rather than the state, and both
    // modal and permission arrive under it. Every other cause is a state
    // name, and serde owns that spelling for the whole wire, so reading it
    // back through serde is what stops this becoming a fourth name table.
    let named = cause.split_once(':').map_or(cause, |(head, _)| head);
    let de: StrDeserializer<ValueError> = named.into_deserializer();
    named == "blocked" || AgentState::deserialize(de).is_ok_and(AgentState::is_blocked)
}

/// The one name that identifies a pane across its whole life.
///
/// The daemon emits fused state under the pane id until the pane is
/// adopted and under the label afterward, so the emitted target is two
/// different strings for one pane. Keying it verbatim left a pre-adoption
/// item that no later event could clear. The pane id is the stable half
/// and every state source carries it; the target is the fallback for a
/// record that does not.
pub fn agent_key<'a>(target: &'a str, pane_id: Option<&'a str>) -> &'a str {
    pane_id.unwrap_or(target)
}

/// One pane as a snapshot answer describes it: the key it is held under,
/// the name it currently wears, and where it stands.
///
/// Named fields because the first two are both `String` and the register
/// keys on one of them. A positional pair transposed silently, and the
/// symptom was an item nobody could clear rather than a compile error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSnapshot {
    /// The stable half of [`agent_key`], and the register's key.
    pub pane_id: String,
    /// Cyclops label, or the pane id while the pane is unadopted.
    pub name: String,
    pub state: AgentState,
}

/// One thing a human must deal with, identified and stated in the same
/// value: every surface renders these and none of them recomputes what
/// belongs on the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionItem {
    /// One pane, identified by [`agent_key`] and named by the newest
    /// record that mentioned it: "%1" before adoption, "reviewer" after.
    Agent {
        pane_id: String,
        name: String,
        state: AgentState,
    },
    /// One delivery, identified by (recipient identity, message id). Only
    /// this delivery's own next transition may clear it, so a later message
    /// to the same recipient cannot close an unresolved one.
    Delivery {
        /// The durable identity this row is keyed under.
        recipient: DeliveryRecipientIdentity,
        /// The label the row was addressed to, for display only.
        to: String,
        id: String,
        state: DeliveryState,
    },
}

/// Who a delivery row is for, as the durable thing a surface may key on.
///
/// `Exact` is the recipient key the record carries. `LegacyLabel` exists
/// only for rows written before durable endpoint identity, whose only name
/// is the label they were addressed to. Display labels are never a key: two
/// exact recipients may share one label, and a rename changes it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeliveryRecipientIdentity {
    Exact(RecipientKey),
    LegacyLabel(String),
}

impl DeliveryRecipientIdentity {
    pub fn of(delivery: &OpenDelivery) -> Self {
        Self::from_parts(delivery.recipient, &delivery.to)
    }

    pub fn from_parts(recipient: Option<RecipientKey>, label: &str) -> Self {
        match recipient {
            Some(key) => Self::Exact(key),
            None => Self::LegacyLabel(label.to_string()),
        }
    }
}

/// The structured key one attention item stands under. Surfaces that dedup,
/// match a line to an item, or ask for a clearance use this and nothing
/// else; a display label cannot become one except through the register's
/// own resolution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AttentionKey {
    Agent {
        pane_id: String,
    },
    Delivery {
        recipient: DeliveryRecipientIdentity,
        id: String,
    },
}

/// How an item stopped needing a human. Rule 3: the reader is owed the
/// difference, because only one of these means somebody dealt with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clearance {
    /// A later transition took the item out of the rule: the prompt was
    /// answered, the delivery was requeued, the daemon's answer no longer
    /// counts it.
    Moved,
    /// The pane left the tmux table while it was blocked. Nobody answered
    /// the prompt; the thing that needed a human went away with the pane,
    /// and saying "cleared" for that would be a claim the record does not
    /// support.
    PaneGone,
}

/// One thing that stopped needing a human, and how. Rule 3's value.
///
/// Carries the item exactly as the register last held it, so the surface
/// rendering the clearance wears the same name and the same cell the alarm
/// row wore and the reader matches the two by sight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub was: AttentionItem,
    pub how: Clearance,
}

/// Which half of the rule raised an item. Ordered, because it is also the
/// tiebreak when one name has items from both halves: the pane comes
/// first, then the deliveries addressed to it, which is the order a human
/// clears them in (an unblocked pane is what lets a delivery move).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Half {
    Agent,
    Delivery,
}

impl AttentionItem {
    /// The name a surface prints: the agent, or the recipient.
    pub fn name(&self) -> &str {
        match self {
            AttentionItem::Agent { name, .. } => name,
            AttentionItem::Delivery { to, .. } => to,
        }
    }

    /// What a record line must NAME to be about this item: the half, then
    /// the one or two strings the register keys on ([`agent_key`] for a
    /// pane, (recipient, message id) for a delivery). Identity only, so a
    /// surface can index its own lines by it in one pass.
    pub fn identity(&self) -> AttentionKey {
        match self {
            AttentionItem::Agent { pane_id, .. } => AttentionKey::Agent {
                pane_id: pane_id.clone(),
            },
            AttentionItem::Delivery { recipient, id, .. } => AttentionKey::Delivery {
                recipient: recipient.clone(),
                id: id.clone(),
            },
        }
    }

    /// Sort position: by name first, so a backlog always reads the same
    /// way, then by half and identity so two items about one agent hold an
    /// order.
    fn sort_key(&self) -> (&str, Half, &str) {
        match self {
            AttentionItem::Agent { pane_id, name, .. } => (name, Half::Agent, pane_id),
            AttentionItem::Delivery { to, id, .. } => (to, Half::Delivery, id),
        }
    }
}

/// The register: everything that needs a human, and nothing else.
///
/// Agents are keyed by pane and hold their latest state, blocked or not:
/// one pane has one current state, and its next transition answers for it.
/// Deliveries hold only the unresolved ones, so the map stays the size of
/// the backlog and never the size of the stream.
#[derive(Debug, Clone, Default)]
pub struct Attention {
    agents: BTreeMap<String, (String, AgentState)>,
    /// Legacy session-ledger deliveries that still need a human, keyed by
    /// durable identity plus message id; the value keeps the display label.
    deliveries: BTreeMap<(DeliveryRecipientIdentity, String), (String, DeliveryState)>,
    /// Durable mailbox attention (open attempts and held queue heads), keyed
    /// the same way and kept apart from the legacy half: a snapshot replaces
    /// only what the mailbox knows, a ledger event only what the ledger knows.
    mailbox: BTreeMap<(DeliveryRecipientIdentity, String), (String, DeliveryState)>,
    /// Both delivery halves as one map by key, the mailbox half overriding
    /// a legacy row with the same key and a legacy-label row dropped when
    /// exactly one exact row carries its label and message. Rebuilt once per
    /// mutation of either half, so every query (count, items, resolution,
    /// clearance) is a lookup, not a rebuild: surfaces call those per row.
    union: BTreeMap<(DeliveryRecipientIdentity, String), (String, DeliveryState)>,
}

impl Attention {
    /// The pane half of the register alone, for an answer that carried no
    /// open deliveries.
    ///
    /// `cyclops status` asks for open deliveries and builds
    /// [`Attention::from_status`], so its eye counts the durable mailbox
    /// half and the legacy delivery half as well as blocked panes. This
    /// constructor exists for callers that did not ask, and makes that
    /// narrower scope explicit instead of relying on an omitted request
    /// parameter.
    pub fn from_live_status(res: &StatusResult) -> Attention {
        let mut attention = Attention::default();
        attention.snapshot_agents(res.sessions.iter().flat_map(|session| &session.panes).map(
            |pane| PaneSnapshot {
                pane_id: pane.pane_id.clone(),
                name: pane.display_name().to_string(),
                state: pane.state,
            },
        ));
        attention
    }

    /// The register the daemon's whole `status` answer describes.
    ///
    /// The stream UI normalizes the same answer into its own seed because
    /// it also needs the pane roster for focus jumps, then calls the same
    /// three snapshot methods as this constructor.
    ///
    /// Scope note: the delivery half is exactly what the answer carried.
    /// `open_deliveries` rides `status` only when the caller asks for it
    /// ([`crate::StatusParams`]), so an answer that did not carry the
    /// backlog yields the pane half alone. The count then understates,
    /// which is the safe direction for an alarm, and the surface can say
    /// so because it holds the same answer.
    pub fn from_status(res: &StatusResult) -> Attention {
        let mut attention = Attention::from_live_status(res);
        attention.snapshot_deliveries(&res.open_deliveries);
        attention.snapshot_mailbox(&res.mailbox_attention);
        attention
    }

    /// Replace the pane roster with the daemon's current one.
    ///
    /// Wholesale, not merged: a pane the answer does not list no longer
    /// exists, and a blocked item for it must stop counting. Merging left
    /// a vanished pane counted for the life of the process, with no later
    /// event able to clear it.
    pub fn snapshot_agents(&mut self, panes: impl IntoIterator<Item = PaneSnapshot>) {
        self.agents = panes
            .into_iter()
            .map(|p| (p.pane_id, (p.name, p.state)))
            .collect();
    }

    /// Replace the delivery backlog with the daemon's folded answer.
    ///
    /// Wholesale for the same reason: the fold covers the whole record,
    /// so anything it does not list has been resolved or never existed.
    pub fn snapshot_deliveries(&mut self, open: &[OpenDelivery]) {
        self.deliveries = Self::keyed(open);
        self.rebuild_union();
    }

    /// Replace the mailbox half from an authenticated snapshot's rows. A key
    /// the previous mailbox half carried and this snapshot does not is an
    /// authoritative absence: the current legacy twin of that key is removed
    /// with it, so the count drops to what the record says now. Nothing is
    /// remembered beyond that: a later live attention fact for the same key
    /// may reopen it, because the event schema cannot prove that fact is the
    /// old attempt, and the register keeps no unbounded memory.
    pub fn snapshot_mailbox(&mut self, rows: &[OpenDelivery]) {
        let next = Self::keyed(rows);
        for (key, (label, _)) in &self.mailbox {
            if next.contains_key(key) {
                continue;
            }
            // The same exact key in the legacy half goes with it.
            self.deliveries.remove(key);
            // So does the legacy twin of the dropped key: a legacy-label row
            // this exact row had canonicalized (same label and message,
            // this being the only exact row carrying that label) was the
            // same delivery, and an authoritative absence ends it too. No
            // memory is kept; a later live legacy event may reopen it.
            if let DeliveryRecipientIdentity::Exact(_) = &key.0 {
                let twin = (
                    DeliveryRecipientIdentity::LegacyLabel(label.clone()),
                    key.1.clone(),
                );
                let another_exact = self.mailbox.iter().chain(self.deliveries.iter()).any(
                    |((identity, id), (row_label, _))| {
                        identity != &key.0
                            && matches!(identity, DeliveryRecipientIdentity::Exact(_))
                            && id == &key.1
                            && row_label == label
                    },
                );
                if !another_exact {
                    self.deliveries.remove(&twin);
                }
            }
        }
        self.mailbox = next;
        self.rebuild_union();
    }

    fn keyed(
        rows: &[OpenDelivery],
    ) -> BTreeMap<(DeliveryRecipientIdentity, String), (String, DeliveryState)> {
        rows.iter()
            .filter(|d| delivery_needs_human(d.state))
            .map(|d| {
                (
                    (DeliveryRecipientIdentity::of(d), d.id.clone()),
                    (d.to.clone(), d.state),
                )
            })
            .collect()
    }

    fn delivery_union(
        &self,
    ) -> &BTreeMap<(DeliveryRecipientIdentity, String), (String, DeliveryState)> {
        &self.union
    }

    /// Recompute the union after either half changed. O(n log n) once per
    /// mutation; queries then never pay for it.
    fn rebuild_union(&mut self) {
        let mut union: BTreeMap<(DeliveryRecipientIdentity, String), (String, DeliveryState)> =
            self.deliveries.clone();
        for (key, value) in &self.mailbox {
            union.insert(key.clone(), value.clone());
        }
        // Semantic twins: a legacy-label row whose label and message exactly
        // one exact row carries is the same delivery; the exact row stands.
        let mut exact_by_label: BTreeMap<(String, String), usize> = BTreeMap::new();
        for ((identity, id), (label, _)) in &union {
            if matches!(identity, DeliveryRecipientIdentity::Exact(_)) {
                *exact_by_label
                    .entry((label.clone(), id.clone()))
                    .or_default() += 1;
            }
        }
        let twins: Vec<_> = union
            .keys()
            .filter(|(identity, id)| match identity {
                DeliveryRecipientIdentity::LegacyLabel(label) => {
                    exact_by_label.get(&(label.clone(), id.clone())) == Some(&1)
                }
                DeliveryRecipientIdentity::Exact(_) => false,
            })
            .cloned()
            .collect();
        for key in &twins {
            union.remove(key);
        }
        self.union = union;
    }

    /// The one exact row carrying `label` for `id`, if exactly one does.
    fn unique_exact_in(
        union: &BTreeMap<(DeliveryRecipientIdentity, String), (String, DeliveryState)>,
        label: &str,
        id: &str,
    ) -> Option<DeliveryRecipientIdentity> {
        let mut exact = union
            .iter()
            .filter(|((identity, entry_id), (row_label, _))| {
                entry_id == id
                    && row_label == label
                    && matches!(identity, DeliveryRecipientIdentity::Exact(_))
            })
            .map(|((identity, _), _)| identity.clone());
        match (exact.next(), exact.next()) {
            (Some(identity), None) => Some(identity),
            _ => None,
        }
    }

    /// Resolve a delivery reference to the key it stands under. An exact
    /// recipient is its own key. A label resolves to an exact row only when
    /// exactly one exact recipient in either half carries that label for
    /// that message; none, or more than one (an alias collision), keeps the
    /// legacy form, so an ambiguous reference can never merge two exact
    /// recipients.
    fn resolve(
        &self,
        recipient: Option<RecipientKey>,
        to: &str,
        id: &str,
    ) -> (DeliveryRecipientIdentity, String) {
        if let Some(key) = recipient {
            return (DeliveryRecipientIdentity::Exact(key), id.to_string());
        }
        let legacy = (
            DeliveryRecipientIdentity::LegacyLabel(to.to_string()),
            id.to_string(),
        );
        let union = self.delivery_union();
        // One exact row carrying this label for this message is the row the
        // reference names, even when a legacy twin also exists; the legacy
        // form is the answer only when no exact row, or more than one,
        // carries the label.
        match Self::unique_exact_in(union, to, id) {
            Some(identity) => (identity, id.to_string()),
            None => legacy,
        }
    }

    pub fn observe_agent(
        &mut self,
        target: &str,
        pane_id: Option<&str>,
        state: AgentState,
    ) -> Option<Resolved> {
        let key = agent_key(target, pane_id).to_string();
        let previous = self.agents.insert(key.clone(), (target.to_string(), state));
        // One blocked state replacing another is not a clearance: the pane
        // still needs a human and the newest row is the one standing.
        let (name, was) = previous.filter(|(_, s)| s.is_blocked() && !state.is_blocked())?;
        Some(Resolved {
            was: AttentionItem::Agent {
                pane_id: key,
                name,
                state: was,
            },
            how: Clearance::Moved,
        })
    }

    /// One pane's last transition: it is gone.
    ///
    /// The tmux table no longer lists it, so nothing will ever report its
    /// state again. The snapshot that would otherwise drop it is taken
    /// once, at startup ([`Attention::snapshot_agents`]) and never on a
    /// timer, which left a pane that blocked and then closed counted for
    /// the life of the reading process with no event able to clear it.
    ///
    /// Keyed by pane id, the same half of [`agent_key`] every state source
    /// carries. An item raised by a record line that named no pane is not
    /// a pane the watcher tracks and is not this edge's to clear.
    ///
    /// Returns the clearance when the pane was blocked as it went (rule
    /// 3), marked [`Clearance::PaneGone`]: nobody answered the prompt, and
    /// the reader is owed that rather than a line implying somebody did.
    pub fn forget_agent(&mut self, pane_id: &str) -> Option<Resolved> {
        let (name, was) = self
            .agents
            .remove(pane_id)
            .filter(|(_, s)| s.is_blocked())?;
        Some(Resolved {
            was: AttentionItem::Agent {
                pane_id: pane_id.to_string(),
                name,
                state: was,
            },
            how: Clearance::PaneGone,
        })
    }

    /// One delivery's latest state, from a live event.
    ///
    /// An id-less delivery (an older daemon, a hand-written line) falls
    /// back to the empty id, which degrades to one slot per recipient for
    /// that recipient only. Tolerant protocol: never drop the fact.
    ///
    /// Returns the clearance when this transition ended an unresolved one
    /// (rule 3). The map holds only unresolved deliveries, so what came
    /// out of it IS the alarm this line answers.
    /// The key a delivery reference stands under, by the register's own
    /// rule: an exact recipient is its own key; a label reaches an exact row
    /// only when exactly one exact recipient carries it for that message.
    pub fn key_for(&self, recipient: Option<RecipientKey>, to: &str, id: &str) -> AttentionKey {
        let (recipient, id) = self.resolve(recipient, to, id);
        AttentionKey::Delivery { recipient, id }
    }

    /// A live ledger delivery edge. The exact recipient rides on the event
    /// when the record has one; a label alone reaches an exact row only
    /// when the register can resolve it unambiguously. Events move the
    /// legacy half; the mailbox half moves only by snapshot.
    pub fn observe_delivery(
        &mut self,
        to: &str,
        recipient: Option<RecipientKey>,
        id: Option<&str>,
        state: DeliveryState,
    ) -> Option<Resolved> {
        let key = self.resolve(recipient, to, id.unwrap_or_default());
        if delivery_needs_human(state) {
            self.deliveries.insert(key, (to.to_string(), state));
            self.rebuild_union();
            return None;
        }
        let removed = self.deliveries.remove(&key);
        self.rebuild_union();
        let (label, was) = removed?;
        Some(Resolved {
            was: AttentionItem::Delivery {
                recipient: key.0,
                to: label,
                id: key.1,
                state: was,
            },
            how: Clearance::Moved,
        })
    }

    pub fn holds(&self, key: &AttentionKey) -> bool {
        self.clearance(key).is_none()
    }

    pub fn clearance(&self, key: &AttentionKey) -> Option<Clearance> {
        match key {
            AttentionKey::Agent { pane_id } => match self.agents.get(pane_id) {
                Some((_, s)) if s.is_blocked() => None,
                Some(_) => Some(Clearance::Moved),
                // Not on the roster at all: the answer lists the panes
                // that exist, so this one no longer does.
                None => Some(Clearance::PaneGone),
            },
            AttentionKey::Delivery { recipient, id } => {
                let entry = (recipient.clone(), id.clone());
                if self.delivery_union().contains_key(&entry) {
                    None
                } else {
                    Some(Clearance::Moved)
                }
            }
        }
    }

    pub fn count(&self) -> usize {
        self.agents.values().filter(|(_, s)| s.is_blocked()).count() + self.delivery_union().len()
    }

    /// Every item, sorted by name so the same backlog always reads the
    /// same way on every surface.
    pub fn items(&self) -> Vec<AttentionItem> {
        let mut items: Vec<AttentionItem> = self
            .agents
            .iter()
            .filter(|(_, (_, s))| s.is_blocked())
            .map(|(pane_id, (name, state))| AttentionItem::Agent {
                pane_id: pane_id.clone(),
                name: name.clone(),
                state: *state,
            })
            .chain(
                self.delivery_union()
                    .iter()
                    .map(|((recipient, id), (to, state))| AttentionItem::Delivery {
                        recipient: recipient.clone(),
                        to: to.clone(),
                        id: id.clone(),
                        state: *state,
                    }),
            )
            .collect();
        items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        items
    }

    /// Where the eye stands for this register.
    pub fn eye(&self) -> Eye {
        Eye::for_count(self.count())
    }

    /// The eye as a header cell, drawn at the position this register puts
    /// it. What `cyclops status` and the plain follow wear.
    pub fn header(&self) -> EyeHeader {
        self.header_drawn(self.eye())
    }

    /// The same cell drawn with a glyph the register did not pick. Only
    /// the stream needs it: its eye ticks through one intermediate frame
    /// per change, and while it does, the alarm and the count still answer
    /// to the register rather than to the glyph on screen.
    pub fn header_drawn(&self, drawn: Eye) -> EyeHeader {
        let count = self.count();
        let tail = match count {
            0 => None,
            1 => Some("1 needs attention".to_string()),
            n => Some(format!("{n} need attention")),
        };
        EyeHeader {
            calm: count == 0,
            cell: if count == 0 {
                drawn.glyph().to_string()
            } else {
                format!("{} {count}", drawn.glyph())
            },
            spoken: match &tail {
                None => format!("eye {}", drawn.word()),
                Some(tail) => format!("eye {} · {tail}", drawn.word()),
            },
            tail,
        }
    }
}

/// The eye as a header cell, composed once for every surface that wears
/// it. Surfaces paint it and place it; they never phrase it.
///
/// Mirroring this composition is how two crates drifted into two eyes with
/// no shared assertion, so a one-sided edit passed both suites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EyeHeader {
    /// Paint with the eye.calm token when true, eye.alert otherwise.
    /// Answers to the count and not to the glyph: a mid-tick eye over a
    /// real backlog is still an alarm.
    pub calm: bool,
    /// The painted cell: the glyph alone when calm, glyph plus count
    /// otherwise ("◑ 1").
    pub cell: String,
    /// The whole header as one line of words, no glyph and no color:
    /// "eye closed", "eye opening · 1 needs attention". What `--plain`
    /// and screen readers wear. A surface with no header to point at
    /// still may not phrase its own; it appends what the items are.
    pub spoken: String,
    /// The words that follow the surface's own middle section, or None
    /// when nothing needs a human.
    pub tail: Option<String>,
}

/// Eye glyphs, chosen for clean single-column rendering in common
/// terminal fonts. The color half is the theme's eye.calm / eye.alert
/// tokens (src/cyclops-ui/src/theme.rs).
///
/// - closed:  `‿` (U+203F undertie), a closed lid. Calm.
/// - opening: `◑` (U+25D1 half circle), the lid lifting. One item.
/// - open:    `◉` (U+25C9 fisheye), wide open, also the cyclops mark.
pub const EYE_CLOSED: &str = "‿";
pub const EYE_OPENING: &str = "◑";
pub const EYE_OPEN: &str = "◉";

/// The eye's three positions. Progression is attention-count driven:
/// 0 closed, 1 opening, 2+ open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Eye {
    Closed,
    Opening,
    Open,
}

impl Eye {
    pub fn glyph(self) -> &'static str {
        match self {
            Eye::Closed => EYE_CLOSED,
            Eye::Opening => EYE_OPENING,
            Eye::Open => EYE_OPEN,
        }
    }

    /// The plain word for --plain and screen readers.
    pub fn word(self) -> &'static str {
        match self {
            Eye::Closed => "closed",
            Eye::Opening => "opening",
            Eye::Open => "open",
        }
    }

    pub fn for_count(count: usize) -> Eye {
        match count {
            0 => Eye::Closed,
            1 => Eye::Opening,
            _ => Eye::Open,
        }
    }

    /// One step toward `target`. The eye ticks through at most one
    /// intermediate frame per state change and never loops.
    pub fn step_toward(self, target: Eye) -> Eye {
        use Eye::*;
        match (self, target) {
            (Closed, Open) => Opening,
            (Open, Closed) => Opening,
            _ => target,
        }
    }
}

impl PaneStatus {
    /// The name every surface calls this pane: its cyclops label, or the
    /// pane id while it is unadopted. One resolution, so the status grid,
    /// the stream, and the register cannot name the same pane differently.
    pub fn display_name(&self) -> &str {
        self.agent.as_deref().unwrap_or(&self.pane_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ComposerProof, ComposerState};
    use crate::wire::{SessionStatus, StatusResult};

    fn pane(pane_id: &str, agent: Option<&str>, state: AgentState) -> PaneStatus {
        PaneStatus {
            pane_id: pane_id.into(),
            window_id: "@1".into(),
            window_name: "agents".into(),
            agent: agent.map(String::from),
            manifest: None,
            title: String::new(),
            current_command: "claude".into(),
            dead: false,
            in_mode: false,
            write_ready: false,
            write_block: None,
            composer: ComposerState::ComposerAmbiguous,
            composer_proof: ComposerProof::Unprovable,
            notification_attempt: None,
            composer_reason: None,
            composer_candidates: 0,
            notification_state: None,
            message_state: None,
            next_action: None,
            width: 120,
            height: 40,
            state,
            state_ms: None,
            working_confirmed: None,
            hooks_verified: None,
            manifest_display_name: None,
            unread: None,
        }
    }

    fn status(panes: Vec<PaneStatus>, open: Vec<OpenDelivery>) -> StatusResult {
        StatusResult {
            daemon_version: "0.1.0".into(),
            daemon_build: None,
            daemon_process: None,
            daemon_executable: None,
            proto: 1,
            boot_id: "b".into(),
            uptime_ms: 1000,
            tmux_version: "3.6a".into(),
            sessions: vec![SessionStatus {
                name: "main".into(),
                attached: true,
                panes,
            }],
            mailbox_routes: Vec::new(),
            admin_unread: 0,
            open_deliveries: open,
            diagnostics: Vec::new(),
            blocked_notifications: Vec::new(),
            blocked_notifications_total: 0,
            manifests: None,
            pid: None,
            mailbox_attention: Vec::new(),
        }
    }

    /// One exact attempt present in both halves is one item: the union
    /// dedups by key with the mailbox overriding, so count, items, and
    /// label resolution never see a false duplicate.
    #[test]
    fn one_exact_key_in_both_halves_is_one_item() {
        use DeliveryState::*;
        let workspace: crate::WorkspaceId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session: crate::SessionInstanceId =
            "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let key = crate::RecipientKey::agent(workspace, session, "%1".parse().unwrap());
        let mut legacy = open("m-1", "worker", ParkedBlockedQuota);
        legacy.recipient = Some(key);
        let mut mailbox = open("m-1", "worker", AttentionRequired);
        mailbox.recipient = Some(key);

        let mut attention = Attention::default();
        attention.snapshot_deliveries(std::slice::from_ref(&legacy));
        attention.snapshot_mailbox(std::slice::from_ref(&mailbox));
        assert_eq!(attention.count(), 1, "one key, one item");
        let items = attention.items();
        assert_eq!(items.len(), 1);
        assert!(
            matches!(&items[0], AttentionItem::Delivery { state, .. } if *state == AttentionRequired),
            "the mailbox half overrides the legacy row: {items:?}"
        );
        let exact = AttentionKey::Delivery {
            recipient: DeliveryRecipientIdentity::Exact(key),
            id: "m-1".into(),
        };
        assert_eq!(
            attention.key_for(None, "worker", "m-1"),
            exact,
            "label resolution stays exact, not falsely ambiguous"
        );
        assert!(attention.holds(&exact));

        // An accepted mailbox absence removes the current legacy twin with
        // it: the count drops to 0 immediately, with no memory kept.
        attention.snapshot_mailbox(&[]);
        assert_eq!(
            attention.count(),
            0,
            "authoritative absence removes the twin"
        );
        assert!(!attention.holds(&exact));
        assert!(attention.items().is_empty());
        // A later live attention fact may reopen the key: the event cannot
        // prove it is the old attempt, and the register remembers nothing,
        // so it is counted again until the record clears it.
        assert!(attention
            .observe_delivery("worker", Some(key), Some("m-1"), ParkedBlockedQuota)
            .is_none());
        assert_eq!(attention.count(), 1, "no permanent suppression");
        assert!(attention
            .observe_delivery("worker", Some(key), Some("m-1"), DeliveredVerified)
            .is_some());
        assert_eq!(attention.count(), 0);
    }

    /// A legacy-label row and one unique exact mailbox row for the same
    /// label and message are semantic twins: one item, resolution exact.
    #[test]
    fn a_legacy_row_and_its_unique_exact_twin_are_one_item() {
        use DeliveryState::*;
        let workspace: crate::WorkspaceId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session: crate::SessionInstanceId =
            "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let key = crate::RecipientKey::agent(workspace, session, "%1".parse().unwrap());
        let legacy = open("m-1", "reviewer", ParkedBlockedQuota); // no key
        let mut mailbox = open("m-1", "reviewer", AttentionRequired);
        mailbox.recipient = Some(key);

        let mut attention = Attention::default();
        attention.snapshot_deliveries(std::slice::from_ref(&legacy));
        attention.snapshot_mailbox(std::slice::from_ref(&mailbox));
        assert_eq!(attention.count(), 1, "semantic twins are one item");
        let items = attention.items();
        assert_eq!(items.len(), 1);
        let exact = AttentionKey::Delivery {
            recipient: DeliveryRecipientIdentity::Exact(key),
            id: "m-1".into(),
        };
        assert_eq!(
            items[0].identity(),
            exact,
            "the exact row is the one that stands"
        );
        assert_eq!(attention.key_for(None, "reviewer", "m-1"), exact);
        assert!(attention.holds(&exact));
        // The legacy form alone is not a standing key while its exact twin exists.
        let legacy_key = AttentionKey::Delivery {
            recipient: DeliveryRecipientIdentity::LegacyLabel("reviewer".into()),
            id: "m-1".into(),
        };
        assert!(!attention.holds(&legacy_key));

        // An accepted mailbox absence ends the pair: the exact row and the
        // legacy twin it canonicalized go together, with no memory kept.
        attention.snapshot_mailbox(&[]);
        assert_eq!(attention.count(), 0, "after absence the pair is gone");
        assert!(attention.items().is_empty());
        assert!(!attention.holds(&exact));
        assert!(!attention.holds(&legacy_key));

        // A later live legacy event may reopen it under the legacy form,
        // because nothing proves it is the old attempt; a live clearance
        // then ends it.
        assert!(attention
            .observe_delivery("reviewer", None, Some("m-1"), ParkedBlockedQuota)
            .is_none());
        assert_eq!(attention.count(), 1, "no permanent suppression");
        assert_eq!(attention.key_for(None, "reviewer", "m-1"), legacy_key);
        assert!(attention
            .observe_delivery("reviewer", None, Some("m-1"), DeliveredVerified)
            .is_some());
        assert_eq!(attention.count(), 0);
    }

    /// The cached union follows every mutation of either half: a seed of
    /// the legacy half, a seed of the mailbox half, a live insert, and a
    /// live clearance each leave count, items, resolution, and clearance
    /// answering for the new state, never the previous one.
    #[test]
    fn the_union_follows_every_mutation() {
        use DeliveryState::*;
        let workspace: crate::WorkspaceId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session: crate::SessionInstanceId =
            "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let key = crate::RecipientKey::agent(workspace, session, "%1".parse().unwrap());
        let exact = AttentionKey::Delivery {
            recipient: DeliveryRecipientIdentity::Exact(key),
            id: "m-1".into(),
        };
        let mut a = Attention::default();
        assert_eq!(a.count(), 0);

        // 1. legacy seed
        let mut legacy = open("m-1", "worker", ParkedBlockedQuota);
        legacy.recipient = Some(key);
        a.snapshot_deliveries(std::slice::from_ref(&legacy));
        assert_eq!(a.count(), 1);
        assert!(a.holds(&exact));
        assert_eq!(a.key_for(None, "worker", "m-1"), exact);

        // 2. mailbox seed overriding the same key
        let mut mailbox = open("m-1", "worker", AttentionRequired);
        mailbox.recipient = Some(key);
        a.snapshot_mailbox(std::slice::from_ref(&mailbox));
        assert_eq!(a.count(), 1);
        assert!(matches!(
            a.items().as_slice(),
            [AttentionItem::Delivery { state, .. }] if *state == AttentionRequired
        ));

        // 3. mailbox absence removes the twin, immediately
        a.snapshot_mailbox(&[]);
        assert_eq!(a.count(), 0);
        assert!(!a.holds(&exact));

        // 4. a live insert reopens it, a live clearance ends it
        assert!(a
            .observe_delivery("worker", Some(key), Some("m-1"), ParkedBlockedQuota)
            .is_none());
        assert_eq!(a.count(), 1);
        assert!(a.holds(&exact));
        assert!(a
            .observe_delivery("worker", Some(key), Some("m-1"), DeliveredVerified)
            .is_some());
        assert_eq!(a.count(), 0);
        assert!(a.items().is_empty());
        assert!(!a.holds(&exact));
    }

    /// Two exact recipients that share a display label are two items, a
    /// label-only reference resolves to a keyed row only when exactly one
    /// row carries that label, and an ambiguous label never merges them.
    #[test]
    fn aliases_never_merge_exact_recipients() {
        use DeliveryState::*;
        let workspace: crate::WorkspaceId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let session: crate::SessionInstanceId =
            "00000000-0000-0000-0000-000000000002".parse().unwrap();
        let first = crate::RecipientKey::agent(workspace, session, "%1".parse().unwrap());
        let second = crate::RecipientKey::agent(workspace, session, "%2".parse().unwrap());
        let mut a = open("m-1", "worker", AttentionRequired);
        a.recipient = Some(first);
        let mut b = open("m-1", "worker", AttentionRequired);
        b.recipient = Some(second);

        let mut attention = Attention::default();
        attention.snapshot_deliveries(&[a.clone(), b.clone()]);
        assert_eq!(attention.count(), 2, "one label, two keys, two items");
        assert_eq!(
            attention.items().len(),
            2,
            "items keep the label for display and stay apart"
        );
        // Ambiguous label: a label-only clearance cannot pick one of them.
        assert!(attention
            .observe_delivery("worker", None, Some("m-1"), DeliveredVerified)
            .is_none());
        assert_eq!(attention.count(), 2);

        // Unambiguous label: the keyed row is the one the event names, and
        // the key it resolves to is the exact recipient, never the label.
        let mut only = Attention::default();
        only.snapshot_deliveries(&[a]);
        assert_eq!(only.count(), 1);
        let exact = AttentionKey::Delivery {
            recipient: DeliveryRecipientIdentity::Exact(first),
            id: "m-1".into(),
        };
        assert_eq!(only.key_for(None, "worker", "m-1"), exact);
        assert!(only.holds(&exact));
        assert!(
            !only.holds(&AttentionKey::Delivery {
                recipient: DeliveryRecipientIdentity::LegacyLabel("worker".into()),
                id: "m-1".into(),
            }),
            "a label is not a key: the row stands under its exact recipient"
        );
        let cleared = only
            .observe_delivery("worker", None, Some("m-1"), DeliveredVerified)
            .expect("the keyed row clears through its label");
        assert_eq!(cleared.was.identity(), exact);
        assert_eq!(only.count(), 0);

        // A legacy row without a key keeps the label form end to end.
        let mut legacy = Attention::default();
        legacy.snapshot_deliveries(&[open("m-2", "reviewer", ParkedBlockedQuota)]);
        assert!(legacy.holds(&AttentionKey::Delivery {
            recipient: DeliveryRecipientIdentity::LegacyLabel("reviewer".into()),
            id: "m-2".into()
        }));
        assert!(legacy
            .observe_delivery("reviewer", None, Some("m-2"), DeliveredVerified)
            .is_some());
    }

    fn open(id: &str, to: &str, state: DeliveryState) -> OpenDelivery {
        OpenDelivery {
            id: id.into(),
            to: to.into(),
            recipient: None,
            state,
            ts: 1000,
            cause: None,
            attempt_id: None,
        }
    }

    #[test]
    fn the_rule_names_exactly_the_states_nothing_else_clears() {
        use DeliveryState::*;
        for s in [AttentionRequired, ParkedBlockedQuota] {
            assert!(delivery_needs_human(s), "{s:?} is an operator's to clear");
        }
        for s in [
            Queued,
            Gating,
            Pasting,
            Staged,
            Submitted,
            DeliveredVerified,
            DeliveredUnverified,
            RetryQueued,
        ] {
            assert!(!delivery_needs_human(s), "{s:?} resolves itself");
        }
        for s in [
            AgentState::BlockedModal,
            AgentState::BlockedPermission,
            AgentState::BlockedQuota,
        ] {
            assert!(s.is_blocked(), "{s} needs a human");
        }
        for s in [
            AgentState::Unknown,
            AgentState::Idle,
            AgentState::IdleWithInput,
            AgentState::Working,
            AgentState::Dead,
        ] {
            assert!(!s.is_blocked(), "{s} is nobody's to clear");
        }
    }

    #[test]
    fn a_pre_write_mailbox_cause_round_trips_through_one_protocol_encoding() {
        let encoded = delivery_pre_write_cause("binding_unprovable");
        assert_eq!(encoded, "blocked_pre_write:binding_unprovable");
        assert_eq!(
            delivery_pre_write_reason(&encoded),
            Some("binding_unprovable")
        );
        assert_eq!(delivery_pre_write_reason("blocked_pre_write:"), None);
        assert_eq!(delivery_pre_write_reason("verify_failed"), None);
    }

    #[test]
    fn a_status_answer_is_both_halves_of_the_count() {
        let res = status(
            vec![
                pane("%1", Some("reviewer"), AgentState::BlockedPermission),
                pane("%2", Some("implementer"), AgentState::Idle),
                pane("%4", None, AgentState::Unknown),
            ],
            vec![open(
                "m-park",
                "implementer",
                DeliveryState::ParkedBlockedQuota,
            )],
        );
        let a = Attention::from_status(&res);
        assert_eq!(a.count(), 2);
        assert_eq!(a.eye(), Eye::Open);
        // Sorted by name, whichever half each item came from.
        assert_eq!(
            a.items(),
            vec![
                AttentionItem::Delivery {
                    recipient: DeliveryRecipientIdentity::LegacyLabel("implementer".to_string()),
                    to: "implementer".into(),
                    id: "m-park".into(),
                    state: DeliveryState::ParkedBlockedQuota,
                },
                AttentionItem::Agent {
                    pane_id: "%1".into(),
                    name: "reviewer".into(),
                    state: AgentState::BlockedPermission,
                },
            ]
        );
        // An unadopted pane answers under its pane id, one resolution.
        assert_eq!(res.sessions[0].panes[2].display_name(), "%4");
    }

    #[test]
    fn live_status_excludes_durable_delivery_alarms() {
        let res = status(
            vec![pane("%1", Some("reviewer"), AgentState::BlockedPermission)],
            vec![open(
                "m-park",
                "implementer",
                DeliveryState::ParkedBlockedQuota,
            )],
        );
        let attention = Attention::from_live_status(&res);
        assert_eq!(attention.count(), 1);
        assert!(matches!(
            attention.items().as_slice(),
            [AttentionItem::Agent { pane_id, .. }] if pane_id == "%1"
        ));
    }

    /// The snapshot replaces both halves. A pane that disappears between
    /// two answers stops counting, and so does a delivery the fold no
    /// longer lists: merging left items nothing could ever clear.
    #[test]
    fn a_snapshot_drops_what_the_daemon_no_longer_lists() {
        let mut a = Attention::from_status(&status(
            vec![pane("%1", Some("reviewer"), AgentState::BlockedModal)],
            vec![open("m-1", "reviewer", DeliveryState::AttentionRequired)],
        ));
        assert_eq!(a.count(), 2);
        // The pane is gone from the rig and the delivery was requeued.
        a.snapshot_agents(Vec::new());
        a.snapshot_deliveries(&[]);
        assert_eq!(a.count(), 0);
        assert_eq!(a.eye(), Eye::Closed);
        assert!(a.items().is_empty());
    }

    #[test]
    fn a_pane_answers_for_its_own_item_across_adoption() {
        let mut a = Attention::default();
        a.observe_agent("%1", Some("%1"), AgentState::BlockedPermission);
        assert_eq!(a.count(), 1);
        assert_eq!(a.items()[0].name(), "%1");
        // Same pane, now labeled: its next state answers for the item and
        // the newest line owns the display name.
        a.observe_agent("reviewer", Some("%1"), AgentState::BlockedModal);
        assert_eq!(a.count(), 1);
        assert_eq!(a.items()[0].name(), "reviewer");
        a.observe_agent("reviewer", Some("%1"), AgentState::Idle);
        assert_eq!(a.count(), 0);
        // A pane-less line still lands: the target is the only name it has.
        a.observe_agent("ghost", None, AgentState::BlockedQuota);
        assert_eq!(a.count(), 1);
        a.observe_agent("ghost", None, AgentState::Idle);
        assert_eq!(a.count(), 0);
    }

    #[test]
    fn only_a_deliverys_own_transition_clears_it() {
        let mut a = Attention::default();
        a.observe_delivery(
            "reviewer",
            None,
            Some("m-1"),
            DeliveryState::AttentionRequired,
        );
        a.observe_delivery("reviewer", None, Some("m-2"), DeliveryState::Queued);
        a.observe_delivery(
            "reviewer",
            None,
            Some("m-2"),
            DeliveryState::DeliveredVerified,
        );
        assert_eq!(a.count(), 1, "m-2 closed m-1's item");
        a.observe_delivery("reviewer", None, Some("m-1"), DeliveryState::Queued);
        assert_eq!(a.count(), 0);
        // An id-less delivery degrades to one slot for that recipient.
        a.observe_delivery("reviewer", None, None, DeliveryState::ParkedBlockedQuota);
        assert_eq!(a.count(), 1);
        a.observe_delivery("reviewer", None, None, DeliveryState::Queued);
        assert_eq!(a.count(), 0);
    }

    /// Header words are composed here and nowhere else, so surfaces with
    /// different scopes still use one eye vocabulary.
    #[test]
    fn the_header_cell_carries_the_count_beside_the_glyph() {
        let mut a = Attention::default();
        let h = a.header();
        assert!(h.calm);
        assert_eq!(h.cell, "‿");
        assert_eq!(h.spoken, "eye closed");
        assert_eq!(h.tail, None);

        a.observe_agent("reviewer", Some("%1"), AgentState::BlockedPermission);
        let h = a.header();
        assert!(!h.calm);
        assert_eq!(h.cell, "◑ 1");
        assert_eq!(h.spoken, "eye opening · 1 needs attention");
        assert_eq!(h.tail.as_deref(), Some("1 needs attention"));

        a.observe_delivery(
            "implementer",
            None,
            Some("m-1"),
            DeliveryState::ParkedBlockedQuota,
        );
        let h = a.header();
        assert_eq!(h.cell, "◉ 2");
        assert_eq!(h.spoken, "eye open · 2 need attention");
        assert_eq!(h.tail.as_deref(), Some("2 need attention"));

        // Mid-animation the surface draws an earlier glyph; the alarm and
        // the count still answer to the register.
        let h = a.header_drawn(Eye::Opening);
        assert_eq!(h.cell, "◑ 2");
        assert!(!h.calm);
    }

    /// The gate's word for a state maps back onto the state, so the calm
    /// view's judgement and the eye's count cannot part company. A prefix
    /// match answered yes for anything starting with the word and tied
    /// itself to no state at all.
    #[test]
    fn a_gate_cause_needs_a_human_exactly_when_the_state_it_names_does() {
        // The gate's own shorthand for a prompt-blocked pane, with and
        // without the rule that named it.
        assert!(gate_cause_needs_human("blocked:trust_dialog"));
        assert!(gate_cause_needs_human("blocked"));
        // Every state, under its own name: the rule and this agree.
        for s in [
            AgentState::Unknown,
            AgentState::Idle,
            AgentState::IdleWithInput,
            AgentState::Working,
            AgentState::BlockedModal,
            AgentState::BlockedPermission,
            AgentState::BlockedQuota,
            AgentState::Dead,
        ] {
            let name = serde_json::to_value(s).expect("a state serializes");
            let name = name.as_str().expect("as a string");
            assert_eq!(
                gate_cause_needs_human(name),
                s.is_blocked(),
                "{name} parted company with the rule"
            );
        }
        // Causes that are not states, and the one a prefix match invented.
        for cause in [
            "pane_in_mode",
            "session_detached",
            "no_such_pane",
            "no_manifest",
            "pane_dead",
            "blockedfoo",
            "blocked_on_a_future_thing",
        ] {
            assert!(!gate_cause_needs_human(cause), "{cause} raised the eye");
        }
    }

    /// A pane that leaves the table takes its item with it. Nothing else
    /// can: no event arrives for a pane that is gone, and the snapshot is
    /// taken once at startup.
    #[test]
    fn a_pane_that_leaves_the_table_stops_counting() {
        let mut a = Attention::default();
        a.observe_agent("reviewer", Some("%1"), AgentState::BlockedPermission);
        a.observe_delivery(
            "reviewer",
            None,
            Some("m-1"),
            DeliveryState::AttentionRequired,
        );
        assert_eq!(a.count(), 2);
        a.forget_agent("%1");
        assert_eq!(a.count(), 1, "the pane's item outlived the pane");
        assert_eq!(a.items()[0].name(), "reviewer");
        // A pane nobody counted, and a second removal, are both no-ops.
        a.forget_agent("%1");
        a.forget_agent("%77");
        assert_eq!(a.count(), 1);
        // The delivery half is untouched: only its own transition clears it.
        a.observe_delivery("reviewer", None, Some("m-1"), DeliveryState::Queued);
        assert_eq!(a.count(), 0);
    }

    /// Identity is what a surface indexes its own lines by, so it has to
    /// be the same identity the register keys on.
    #[test]
    fn an_items_identity_is_the_key_the_register_holds_it_under() {
        let a = Attention::from_status(&status(
            vec![pane("%1", Some("reviewer"), AgentState::BlockedModal)],
            vec![open("m-1", "implementer", DeliveryState::AttentionRequired)],
        ));
        let items = a.items();
        assert_eq!(
            items[0].identity(),
            AttentionKey::Delivery {
                recipient: DeliveryRecipientIdentity::LegacyLabel("implementer".into()),
                id: "m-1".into(),
            }
        );
        assert_eq!(
            items[1].identity(),
            AttentionKey::Agent {
                pane_id: "%1".into()
            }
        );
        // The pane's key is agent_key's, not the label it currently wears.
        assert_eq!(agent_key("reviewer", Some("%1")), "%1");
    }

    /// Rule 3, agent half: the transition OUT of a blocked state is handed
    /// back so the surface that showed the alarm can show it end. A second
    /// blocked state is not an ending, and neither is anything happening to
    /// a pane that was never blocked.
    #[test]
    fn a_pane_leaving_a_blocked_state_hands_back_the_clearance() {
        let mut a = Attention::default();
        assert_eq!(
            a.observe_agent("reviewer", Some("%1"), AgentState::Working),
            None,
            "a pane that never needed a human cleared something"
        );
        assert_eq!(
            a.observe_agent("reviewer", Some("%1"), AgentState::BlockedPermission),
            None,
            "raising an alarm is not ending one"
        );
        // Still blocked: the pane needs a human, just for another reason.
        assert_eq!(
            a.observe_agent("reviewer", Some("%1"), AgentState::BlockedQuota),
            None,
            "one blocked state replacing another read as an ending"
        );
        assert_eq!(
            a.observe_agent("reviewer", Some("%1"), AgentState::Idle),
            Some(Resolved {
                was: AttentionItem::Agent {
                    pane_id: "%1".into(),
                    name: "reviewer".into(),
                    state: AgentState::BlockedQuota,
                },
                how: Clearance::Moved,
            }),
            "the newest alarm is the one the clearance answers"
        );
        assert_eq!(a.count(), 0);
    }

    /// The clearance wears the name the register HELD, not the one arriving
    /// with the transition. A pane adopted between the two lines writes the
    /// alarm under "%1" and the next state under "reviewer", and a
    /// clearance naming "reviewer" answers a row the reader never saw.
    #[test]
    fn a_clearance_wears_the_name_the_alarm_row_wore() {
        let mut a = Attention::default();
        a.observe_agent("%1", Some("%1"), AgentState::BlockedPermission);
        let resolved = a
            .observe_agent("reviewer", Some("%1"), AgentState::Working)
            .expect("adoption did not lose the clearance");
        assert_eq!(resolved.was.name(), "%1");
        assert_eq!(
            resolved.was.identity(),
            AttentionKey::Agent {
                pane_id: "%1".into()
            }
        );
    }

    /// Rule 3, the pane's other ending: the window closed on the prompt.
    /// Nobody answered it, and a clearance that said otherwise would be a
    /// claim the record does not support.
    #[test]
    fn a_pane_that_goes_while_blocked_says_so() {
        let mut a = Attention::default();
        a.observe_agent("reviewer", Some("%1"), AgentState::BlockedModal);
        assert_eq!(
            a.forget_agent("%1"),
            Some(Resolved {
                was: AttentionItem::Agent {
                    pane_id: "%1".into(),
                    name: "reviewer".into(),
                    state: AgentState::BlockedModal,
                },
                how: Clearance::PaneGone,
            })
        );
        // A pane nobody was waiting on, and a second removal, end nothing.
        a.observe_agent("implementer", Some("%2"), AgentState::Idle);
        assert_eq!(a.forget_agent("%2"), None);
        assert_eq!(a.forget_agent("%1"), None);
    }

    /// Rule 3, delivery half: only this delivery's own next transition ends
    /// it, and what comes back is the state it was stuck in.
    #[test]
    fn a_deliverys_own_transition_hands_back_what_it_ended() {
        let mut a = Attention::default();
        assert_eq!(
            a.observe_delivery("reviewer", None, Some("m-1"), DeliveryState::Queued),
            None
        );
        assert_eq!(
            a.observe_delivery(
                "reviewer",
                None,
                Some("m-1"),
                DeliveryState::ParkedBlockedQuota
            ),
            None
        );
        // Another message to the same recipient ends nothing of m-1's.
        assert_eq!(
            a.observe_delivery(
                "reviewer",
                None,
                Some("m-2"),
                DeliveryState::DeliveredVerified
            ),
            None
        );
        assert_eq!(
            a.observe_delivery("reviewer", None, Some("m-1"), DeliveryState::Queued),
            Some(Resolved {
                was: AttentionItem::Delivery {
                    recipient: DeliveryRecipientIdentity::LegacyLabel("reviewer".to_string()),
                    to: "reviewer".into(),
                    id: "m-1".into(),
                    state: DeliveryState::ParkedBlockedQuota,
                },
                how: Clearance::Moved,
            })
        );
        assert_eq!(a.count(), 0);
    }

    /// The same rule asked of the roster instead of a transition, which is
    /// how a surface reconciles lines it ALREADY shows against the daemon's
    /// answer. Both are one truth: `holds` is this question, negated.
    #[test]
    fn the_register_says_how_an_item_it_no_longer_holds_ended() {
        let a = Attention::from_status(&status(
            vec![
                pane("%1", Some("reviewer"), AgentState::BlockedPermission),
                pane("%2", Some("implementer"), AgentState::Idle),
            ],
            vec![open("m-1", "implementer", DeliveryState::AttentionRequired)],
        ));
        let agent = |pane_id: &str| AttentionKey::Agent {
            pane_id: pane_id.into(),
        };
        let delivery = |to: &str, id: &str| AttentionKey::Delivery {
            recipient: DeliveryRecipientIdentity::LegacyLabel(to.into()),
            id: id.into(),
        };
        let blocked = agent("%1");
        let unblocked = agent("%2");
        let gone = agent("%9");
        let open_one = delivery("implementer", "m-1");
        let requeued = delivery("implementer", "m-2");

        assert_eq!(a.clearance(&blocked), None);
        assert_eq!(a.clearance(&unblocked), Some(Clearance::Moved));
        // Not on the roster: the answer lists the panes that exist.
        assert_eq!(a.clearance(&gone), Some(Clearance::PaneGone));
        assert_eq!(a.clearance(&open_one), None);
        assert_eq!(a.clearance(&requeued), Some(Clearance::Moved));

        for id in [blocked, unblocked, gone, open_one, requeued] {
            assert_eq!(
                a.holds(&id),
                a.clearance(&id).is_none(),
                "{id:?} got two answers to one question"
            );
        }
    }

    #[test]
    fn the_eye_steps_through_one_intermediate_frame() {
        assert_eq!(Eye::for_count(0), Eye::Closed);
        assert_eq!(Eye::for_count(1), Eye::Opening);
        assert_eq!(Eye::for_count(9), Eye::Open);
        assert_eq!(Eye::Closed.step_toward(Eye::Open), Eye::Opening);
        assert_eq!(Eye::Open.step_toward(Eye::Closed), Eye::Opening);
        assert_eq!(Eye::Opening.step_toward(Eye::Open), Eye::Open);
        assert_eq!(Eye::Closed.word(), "closed");
    }
}
