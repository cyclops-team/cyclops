//! What needs a human right now.
//!
//! One rule, one register, one owner. The eye is the signature device
//! (GOALS), and it appears on three surfaces: the stream header, the
//! `--plain` eye line, and `cyclops status`. All three read this module.
//! None of them recomputes it, and the daemon answers `status` with the
//! same predicate it uses here.
//!
//! ## The rule
//!
//! 1. An AGENT needs a human while its fused state is blocked
//!    ([`AgentState::is_blocked`]): a permission prompt, a vendor modal,
//!    or an exhausted quota. Nothing downstream clears any of the three.
//!    A surface that only holds the gate's WORD for that state asks
//!    [`gate_cause_needs_human`], which maps the word back onto the state.
//! 2. A DELIVERY needs a human while its latest recorded state is one the
//!    pipeline cannot leave on its own ([`delivery_needs_human`]):
//!    redelivery exhausted, or a quota park, which never auto-retries
//!    (GOALS, finding F11). Both are terminal in the record until an
//!    operator requeues them.
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
//! 1. The daemon's `status` answer is a SNAPSHOT of now: the pane roster
//!    as the watcher currently sees it, and the open deliveries folded
//!    from the whole record. It REPLACES both halves ([`Attention::
//!    snapshot_agents`], [`Attention::snapshot_deliveries`]), so a pane
//!    that is gone stops counting and an item nothing could clear cannot
//!    outlive the answer.
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
    /// One delivery, identified by (recipient, message id). Only this
    /// delivery's own next transition may clear it, so a later message to
    /// the same recipient cannot close an unresolved one.
    Delivery {
        to: String,
        id: String,
        state: DeliveryState,
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
    pub fn identity(&self) -> (Half, &str, &str) {
        match self {
            AttentionItem::Agent { pane_id, .. } => (Half::Agent, pane_id, ""),
            AttentionItem::Delivery { to, id, .. } => (Half::Delivery, to, id),
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
    deliveries: BTreeMap<(String, String), DeliveryState>,
}

impl Attention {
    /// The register the daemon's whole `status` answer describes.
    ///
    /// `cyclops status` reads this. The stream UI does not: it normalizes
    /// the same answer into its own seed (it needs the pane roster for the
    /// focus jump too) and then calls the same two snapshot methods this
    /// does. Two callers, one pair of snapshot methods, so the two
    /// surfaces cannot count one answer differently.
    ///
    /// Scope note: the delivery half is exactly what the answer carried.
    /// `open_deliveries` rides `status` only when the caller asks for it
    /// ([`crate::StatusParams`]), so an answer that did not carry the
    /// backlog yields the pane half alone. The count then understates,
    /// which is the safe direction for an alarm, and the surface can say
    /// so because it holds the same answer.
    pub fn from_status(res: &StatusResult) -> Attention {
        let mut a = Attention::default();
        a.snapshot_agents(
            res.sessions
                .iter()
                .flat_map(|s| &s.panes)
                .map(|p| PaneSnapshot {
                    pane_id: p.pane_id.clone(),
                    name: p.display_name().to_string(),
                    state: p.state,
                }),
        );
        a.snapshot_deliveries(&res.open_deliveries);
        a
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
        self.deliveries = open
            .iter()
            .filter(|d| delivery_needs_human(d.state))
            .map(|d| ((d.to.clone(), d.id.clone()), d.state))
            .collect();
    }

    /// One pane's latest state, from a live event.
    ///
    /// Stored blocked or not: the pane's next transition is what answers
    /// for its item, and the newest line owns the display name, so an item
    /// raised under "%1" reads as "reviewer" once the pane is adopted.
    ///
    /// Returns the clearance when this state ended a blocked one (rule 3).
    /// The item it hands back wears the name the register held while the
    /// alarm stood, not the one arriving now: the clearance exists to be
    /// matched against the row the reader already saw, and a pane adopted
    /// between the two lines wears a different name on each.
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
    pub fn observe_delivery(
        &mut self,
        to: &str,
        id: Option<&str>,
        state: DeliveryState,
    ) -> Option<Resolved> {
        let key = (to.to_string(), id.unwrap_or_default().to_string());
        if delivery_needs_human(state) {
            self.deliveries.insert(key, state);
            return None;
        }
        let was = self.deliveries.remove(&key)?;
        Some(Resolved {
            was: AttentionItem::Delivery {
                to: key.0,
                id: key.1,
                state: was,
            },
            how: Clearance::Moved,
        })
    }

    /// Does the register still hold this item, by the identity a surface
    /// indexes its own lines under ([`AttentionItem::identity`])?
    ///
    /// For a line that POINTS AT an item rather than being one: the
    /// daemon's admin pings say a human is needed but are not themselves
    /// a state, so nothing can clear them. A ping whose item the register
    /// no longer holds is a ping about something already resolved, and
    /// showing it beside the count put "action required" under a closed
    /// eye (cyclops-ui `App::admits`).
    pub fn holds(&self, id: (Half, &str, &str)) -> bool {
        self.clearance(id).is_none()
    }

    /// Rule 3 asked of the register directly: how would it explain no
    /// longer holding this item? None means it still does.
    ///
    /// For a surface reconciling LINES it already shows against the
    /// daemon's answer, which is the other way an alarm ends. The live
    /// mutators above answer the same question from the transition they
    /// are handed; this answers it from the roster, which is the only
    /// thing that can tell a pane that moved on from a pane that is gone.
    pub fn clearance(&self, (half, name, id): (Half, &str, &str)) -> Option<Clearance> {
        match half {
            Half::Agent => match self.agents.get(name) {
                Some((_, s)) if s.is_blocked() => None,
                Some(_) => Some(Clearance::Moved),
                // Not on the roster at all: the answer lists the panes
                // that exist, so this one no longer does.
                None => Some(Clearance::PaneGone),
            },
            Half::Delivery => match self
                .deliveries
                .contains_key(&(name.to_string(), id.to_string()))
            {
                true => None,
                false => Some(Clearance::Moved),
            },
        }
    }

    /// How many things need a human.
    pub fn count(&self) -> usize {
        self.agents.values().filter(|(_, s)| s.is_blocked()).count() + self.deliveries.len()
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
                self.deliveries
                    .iter()
                    .map(|((to, id), state)| AttentionItem::Delivery {
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
/// tokens (crates/cyclops-ui/src/theme.rs).
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
            width: 120,
            height: 40,
            state,
            state_ms: None,
            hooks_verified: None,
        }
    }

    fn status(panes: Vec<PaneStatus>, open: Vec<OpenDelivery>) -> StatusResult {
        StatusResult {
            daemon_version: "0.1.0".into(),
            proto: 1,
            boot_id: "b".into(),
            uptime_ms: 1000,
            tmux_version: "3.6a".into(),
            sessions: vec![SessionStatus {
                name: "main".into(),
                attached: true,
                panes,
            }],
            open_deliveries: open,
            manifests: None,
            pid: None,
        }
    }

    fn open(id: &str, to: &str, state: DeliveryState) -> OpenDelivery {
        OpenDelivery {
            id: id.into(),
            to: to.into(),
            state,
            ts: 1000,
            cause: None,
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
        a.observe_delivery("reviewer", Some("m-1"), DeliveryState::AttentionRequired);
        a.observe_delivery("reviewer", Some("m-2"), DeliveryState::Queued);
        a.observe_delivery("reviewer", Some("m-2"), DeliveryState::DeliveredVerified);
        assert_eq!(a.count(), 1, "m-2 closed m-1's item");
        a.observe_delivery("reviewer", Some("m-1"), DeliveryState::Queued);
        assert_eq!(a.count(), 0);
        // An id-less delivery degrades to one slot for that recipient.
        a.observe_delivery("reviewer", None, DeliveryState::ParkedBlockedQuota);
        assert_eq!(a.count(), 1);
        a.observe_delivery("reviewer", None, DeliveryState::Queued);
        assert_eq!(a.count(), 0);
    }

    /// The header words are composed here and nowhere else, so the stream,
    /// `cyclops status` and the plain follow cannot phrase the same count
    /// three ways.
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
        a.observe_delivery("reviewer", Some("m-1"), DeliveryState::AttentionRequired);
        assert_eq!(a.count(), 2);
        a.forget_agent("%1");
        assert_eq!(a.count(), 1, "the pane's item outlived the pane");
        assert_eq!(a.items()[0].name(), "reviewer");
        // A pane nobody counted, and a second removal, are both no-ops.
        a.forget_agent("%1");
        a.forget_agent("%77");
        assert_eq!(a.count(), 1);
        // The delivery half is untouched: only its own transition clears it.
        a.observe_delivery("reviewer", Some("m-1"), DeliveryState::Queued);
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
        assert_eq!(items[0].identity(), (Half::Delivery, "implementer", "m-1"));
        assert_eq!(items[1].identity(), (Half::Agent, "%1", ""));
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
        assert_eq!(resolved.was.identity(), (Half::Agent, "%1", ""));
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
            a.observe_delivery("reviewer", Some("m-1"), DeliveryState::Queued),
            None
        );
        assert_eq!(
            a.observe_delivery("reviewer", Some("m-1"), DeliveryState::ParkedBlockedQuota),
            None
        );
        // Another message to the same recipient ends nothing of m-1's.
        assert_eq!(
            a.observe_delivery("reviewer", Some("m-2"), DeliveryState::DeliveredVerified),
            None
        );
        assert_eq!(
            a.observe_delivery("reviewer", Some("m-1"), DeliveryState::Queued),
            Some(Resolved {
                was: AttentionItem::Delivery {
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
        let blocked = (Half::Agent, "%1", "");
        let unblocked = (Half::Agent, "%2", "");
        let gone = (Half::Agent, "%9", "");
        let open_one = (Half::Delivery, "implementer", "m-1");
        let requeued = (Half::Delivery, "implementer", "m-2");

        assert_eq!(a.clearance(blocked), None);
        assert_eq!(a.clearance(unblocked), Some(Clearance::Moved));
        // Not on the roster: the answer lists the panes that exist.
        assert_eq!(a.clearance(gone), Some(Clearance::PaneGone));
        assert_eq!(a.clearance(open_one), None);
        assert_eq!(a.clearance(requeued), Some(Clearance::Moved));

        for id in [blocked, unblocked, gone, open_one, requeued] {
            assert_eq!(
                a.holds(id),
                a.clearance(id).is_none(),
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
