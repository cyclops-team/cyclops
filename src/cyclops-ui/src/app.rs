//! UI state: views, filters, selection, density, the sidebar roster, and
//! the eye animation. Pure state transitions here; no IO, no terminal, so
//! every behavior is unit-testable.
//!
//! The record itself (the entry ring, the attention register, and the
//! calm/firehose decision) is not this file's. [`crate::stream::Record`]
//! owns it, backend-neutral, so a future workspace panel reads the same
//! ordering and the same judgement. `App` holds one and is otherwise this
//! renderer's own state: the sidebar roster and the focus-jump map (both
//! navigation, not the record), and the key handling that turns keyboard
//! and mouse input into that state moving.

use std::collections::{BTreeMap, HashMap};

use cyclops_proto::{Attention, AttentionItem, Eye};

use crate::input::Key;
use crate::stream::{EndpointFilter, Entry, EntryKind, Filter, Record, StatusSeed};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Admin,
    Firehose,
    /// The work queue. A sibling of the stream views, reading the
    /// messages snapshot rather than the event record.
    Messages,
}

impl View {
    pub fn words(self) -> &'static str {
        match self {
            View::Admin => "admin stream",
            View::Firehose => "firehose",
            View::Messages => "messages",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Comfortable,
    Compact,
}

/// One agent row in the sidebar: who, where they stand, since when.
#[derive(Debug)]
pub struct RosterRow {
    pub session_idx: usize,
    pub name: String,
    pub pane_id: String,
    pub state: cyclops_proto::AgentState,
    pub manifest: Option<String>,
    since: Since,
}

/// Where a row's elapsed-in-state comes from. Two honest sources and one
/// honest absence; the sidebar never counts from a guess.
#[derive(Debug)]
enum Since {
    /// The daemon said how long at seed time; the anchor extends it.
    Seeded {
        state_ms: u64,
        at: std::time::Instant,
    },
    /// This process saw the transition itself.
    Observed(std::time::Instant),
    /// Nobody has said. The elapsed cell stays empty.
    Unknown,
}

/// One daemon session slot and one tmux pane id.
///
/// Pane ids stay globally unique on the supported single tmux server, but a
/// transfer crosses two independently scheduled session watchers. Keeping the
/// source slot prevents its late removal from erasing the destination route.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PaneRoute {
    session_idx: usize,
    pane_id: String,
}

impl PaneRoute {
    fn new(session_idx: usize, pane_id: &str) -> Self {
        Self {
            session_idx,
            pane_id: pane_id.to_string(),
        }
    }
}

impl RosterRow {
    /// Milliseconds in the current state, as of now.
    ///
    /// Computed at render, not stored: the value is only ever as fresh as
    /// the frame that shows it, and frames are event-driven (no redraw
    /// timer ticks this along; the zero-polling contract outranks a live
    /// clock, and any event refreshes it).
    pub fn elapsed_ms(&self) -> Option<u64> {
        match self.since {
            Since::Seeded { state_ms, at } => Some(state_ms + at.elapsed().as_millis() as u64),
            Since::Observed(at) => Some(at.elapsed().as_millis() as u64),
            Since::Unknown => None,
        }
    }
}

/// What one screen row means to a click. frame::build writes one per
/// terminal row as it lays the frame out, so the mouse asks the frame
/// that is actually on screen rather than recomputing a layout that
/// might not match it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RowTarget {
    #[default]
    Nothing,
    /// A sidebar agent row: click jumps focus to the pane.
    Agent(String),
    /// A stream entry row: click selects it.
    Entry(u64),
}

/// An open filter input line: which filter, and the buffer so far.
pub struct InputLine {
    pub which: Which,
    pub buf: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    With,
    From,
    To,
}

impl Which {
    /// Every filter, in the order the header and the cheatsheet list them.
    pub const ALL: [Which; 3] = [Which::With, Which::From, Which::To];

    pub fn word(self) -> &'static str {
        match self {
            Which::With => "with",
            Which::From => "from",
            Which::To => "to",
        }
    }

    /// The key that opens this filter's input line.
    ///
    /// `handle_key` binds the same three keys, and
    /// `the_band_names_the_key_that_opens_the_filter_it_names` holds the
    /// two together: the attention band tells a reader which key clears
    /// the filter hiding their line, and naming a fixed one sent every
    /// `--from` and `--to` reader to a key that left their filter alone.
    pub fn key(self) -> char {
        match self {
            Which::With => 'w',
            Which::From => 'f',
            Which::To => 't',
        }
    }

    /// This filter's current value, or None when it is not set.
    pub fn value(self, filter: &Filter) -> Option<&str> {
        match self {
            Which::With => filter.with.as_ref().map(EndpointFilter::label),
            Which::From => filter.from.as_ref().map(EndpointFilter::label),
            Which::To => filter.to.as_ref().map(EndpointFilter::label),
        }
    }
}

/// What a handled key asks the runtime to do beyond redrawing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Quit,
    /// Open a fresh event subscription. Only ever from a keystroke: the
    /// UI does not retry a dead socket on its own.
    Reconnect,
    /// Jump focus to this pane id or agent label.
    Focus(String),
}

pub struct App {
    pub theme: Theme,
    pub view: View,
    pub density: Density,
    pub filter: Filter,
    pub input: Option<InputLine>,
    pub overlay: bool,
    /// Pinned to the tail: arrivals scroll into view. Unpinned: the
    /// viewport holds still and arrivals append below it.
    pub pinned: bool,
    /// Selected entry uid; None means the tail.
    pub selected: Option<u64>,
    /// Anchor uid of the top visible entry while unpinned.
    pub top: Option<u64>,
    /// One-line footer notice (focus errors), replaced on next notice.
    pub notice: Option<String>,
    /// Build identity from the authenticated socket greeting. Unlike a
    /// transient notice, a mismatch survives navigation and key handling.
    pub build_health: Option<crate::health::BuildHealth>,
    /// Set when the daemon connection dies; the header says so.
    pub conn_lost: bool,
    /// The roster panel is on when the terminal is wide enough; `a` hides
    /// it for a session that wants the full width back.
    pub show_roster: bool,
    /// What each terminal row of the last frame means to a click, halved
    /// where the frame is: left of `sidebar_w` is the panel's target,
    /// right of it the stream's. frame::build rewrites both per frame;
    /// handle_key reads them.
    pub row_targets: Vec<(RowTarget, RowTarget)>,
    /// Display columns the sidebar occupied in the last frame; 0 when it
    /// was not drawn.
    pub sidebar_w: usize,
    /// The record: the entry ring, the attention register, stable uids.
    /// Backend-neutral (`crate::stream`); every other field on this struct
    /// is this renderer's own.
    record: Record,
    eye: Eye,
    /// Agent label -> exact route, harvested from status and events only
    /// (zero polling). Backs the focus jump.
    panes: HashMap<String, PaneRoute>,
    /// Every watched pane, keyed by exact route: the sidebar's rows. Seeded
    /// from the one status answer, then moved by live events alone, the
    /// same contract the attention register keeps.
    roster: BTreeMap<PaneRoute, RosterRow>,
    admin_unread: u64,
    /// Status-owned label bindings used by interactive filter input.
    filter_routes: Vec<EndpointFilter>,
    /// The messages work queue. Only the Messages view reads it, and it
    /// is filled by whole snapshot replacement, never patched.
    pub queue: crate::queue::HumanQueue,
    /// Snapshot invalidation and connection state.
    pub refresh: crate::messages::RefreshGate,
    /// The open detail, if the reader opened one.
    pub detail: Option<crate::detail::Detail>,
    /// A request the reader confirmed, resolved at that moment and
    /// waiting for the loop to send it.
    ///
    /// Never a bare action. Resolving at drain time read whatever detail
    /// was open by then, so confirming on one row, leaving, and opening
    /// another built the first row's verb against the second's target.
    pending: Option<(
        crate::action_io::RequestToken,
        crate::action_io::ActionRequest,
    )>,
    /// The one request that is out. Its answer is the only one allowed
    /// to touch the detail.
    in_flight: Option<crate::action_io::RequestToken>,
    /// The size the last frame was drawn at.
    ///
    /// Kept so the key handler can ask the same question the renderer
    /// answered: is there room to show what a destructive key would do.
    /// A key must not act on something the frame could not display.
    pub last_size: (usize, usize),
    /// A reconnect the loop still owes. Set by the key, taken by the
    /// event loop, which is the only place that can spawn a task.
    pub reconnect_owed: bool,
    /// The terminal is mid-paste. Set by PasteStart, cleared by PasteEnd.
    ///
    /// While true nothing between the brackets is interpreted, which is
    /// the only reliable defence: a pasted `q` is a `q` by the time it
    /// reaches the key handler and cannot be told from a typed one.
    pasting: bool,
}

/// One idempotency key per draft.
///
/// A uuid rather than a pid and a counter. The daemon keys idempotency on
/// sender plus client key, and both a pid and a counter reset, so a
/// restarted UI could mint a pair an older journal entry already holds
/// and have a fresh message treated as a repeat of an old one.
fn next_client_key() -> String {
    format!("ui-{}", uuid::Uuid::new_v4())
}

impl App {
    pub fn new(theme: Theme, view: View, filter: Filter) -> App {
        App {
            theme,
            view,
            density: Density::Comfortable,
            filter,
            input: None,
            overlay: false,
            pinned: true,
            selected: None,
            top: None,
            notice: None,
            build_health: None,
            conn_lost: false,
            show_roster: true,
            row_targets: Vec::new(),
            sidebar_w: 0,
            record: Record::new(),
            eye: Eye::Closed,
            panes: HashMap::new(),
            roster: BTreeMap::new(),
            admin_unread: 0,
            filter_routes: Vec::new(),
            queue: crate::queue::HumanQueue::new(),
            refresh: crate::messages::RefreshGate::new(),
            detail: None,
            pending: None,
            in_flight: None,
            pasting: false,
            reconnect_owed: false,
            // Assume a usable frame until one is drawn. A fresh App has
            // not rendered, and refusing every action before the first
            // frame would be its own defect.
            last_size: (80, 24),
        }
    }

    /// Persistent health copy followed by the current transient notice.
    pub fn notice_text(&self) -> Option<String> {
        let health = self
            .build_health
            .as_ref()
            .and_then(crate::health::BuildHealth::notice);
        match (health, self.notice.as_deref()) {
            (Some(health), Some(notice)) => Some(format!("{health}; {notice}")),
            (Some(health), None) => Some(health),
            (None, Some(notice)) => Some(notice.to_string()),
            (None, None) => None,
        }
    }

    /// The request that is out, if any.
    pub fn in_flight(&self) -> Option<&crate::action_io::RequestToken> {
        self.in_flight.as_ref()
    }

    /// Is a confirmed request waiting to be sent?
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Does an open detail still owe a read?
    pub fn detail_read_owed(&self) -> bool {
        self.detail
            .as_ref()
            .is_some_and(|d| matches!(d.stage(), crate::detail::Stage::Opening) || d.needs_reload())
    }

    /// Apply what came back, if it belongs to what is on screen.
    ///
    /// Two checks and both are load bearing. The token must be the one
    /// still in flight, which drops an answer to a request the reader
    /// has moved on from. And its target must be the detail's, which is
    /// the privacy half: without it, closing one detail and opening
    /// another lets the first message's body render in the second's
    /// frame. A response failing either check is dropped whole, and
    /// nothing about it reaches the screen.
    pub fn apply_action(
        &mut self,
        token: crate::action_io::RequestToken,
        outcome: crate::action_io::ActionOutcome,
    ) {
        use crate::action_io::ActionOutcome;
        if self.in_flight.as_ref() != Some(&token) {
            return;
        }
        self.in_flight = None;
        let action = token.action();
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        // BOTH halves. The row decides which detail may apply this, and
        // survives an alarm appearing or clearing underneath it. The
        // attempt decides what the answer was about: a requeue replaces
        // it, and an answer naming the old attempt must not land on a
        // detail now looking at the new one.
        if detail.target().target != *token.row() || detail.target().attempt != token.attempt() {
            return;
        }
        match outcome {
            ActionOutcome::Opened(loaded) => detail.loaded_ok(*loaded),
            ActionOutcome::Done(note) => match action {
                Some(action) => {
                    detail.done(action, note);
                    // What the daemon holds has moved, so the list is
                    // owed a read as well.
                    self.refresh.mark_dirty();
                }
                None => detail.loaded_ok(crate::detail::Loaded::default()),
            },
            ActionOutcome::Refused { code, message } => detail.refused(action, &code, message),
            ActionOutcome::NotSent(why) => detail.not_sent(action, why),
            // A read is not automatically harmless. An open that claims
            // is a mutation, so its unanswered request is unknown rather
            // than absent, and must never be reported as nothing having
            // changed. A claim is safely reopenable, which is why the
            // detail stays usable either way.
            ActionOutcome::Uncertain(why) => detail.uncertain(action, why),
        }
    }

    /// Open the selected row, freezing its target and the watermark.
    ///
    /// Refused while a detail is already open, which is what stops a
    /// second Enter opening a second read against the same row.
    pub fn open_detail(&mut self) -> Option<()> {
        use crate::detail::Detail;

        // Refused while a detail is open, which is what stops a second
        // Enter starting a second read. Not refused merely because a
        // request is out: leaving a slow read and opening something else
        // has to work, or a reader is stuck until it times out. The
        // answer to the abandoned read is dropped on its token.
        if self.detail.is_some() {
            return None;
        }
        let row = self.queue.selected()?.clone();
        self.detail = Some(Detail::open(&row, self.queue.watermark()));
        Some(())
    }

    /// The read an open detail is owed, with the token naming it.
    ///
    /// One place decides what a detail's read looks like, so opening it
    /// and reloading it after its facts moved cannot drift apart.
    pub fn detail_read(
        &self,
    ) -> Option<(
        crate::action_io::RequestToken,
        crate::action_io::ActionRequest,
    )> {
        use crate::action_io::{ActionRequest, RequestKind, RequestToken};
        use crate::queue::{Direction, MailboxWord};

        let detail = self.detail.as_ref()?;
        let frozen = detail.target().clone();
        // One read per open. Fresh recovery follows daemon authorization.
        // An uncertain intent opens the exact attempt for inspection.
        // Only a matching durable terminal-accepted fact can expose
        // no-key reconciliation of that recorded verb.
        let (request, claims) = match frozen.attempt {
            Some(attempt_id)
                if detail.can_manage_attention()
                    || detail.resolution_intent().is_some()
                    || detail.resolution_action_accepted().is_some()
                    || detail.resolution_consumption_observed().is_some() =>
            {
                (ActionRequest::OpenAttention { attempt_id }, false)
            }
            _ => {
                // Claim only what is genuinely yours and still waiting,
                // and only on the first open. Observing somebody else's
                // mailbox must not take it, and a reload is a read.
                let claims = matches!(detail.stage(), crate::detail::Stage::Opening)
                    && detail.direction() == Direction::Inbound
                    && detail.mailbox() == MailboxWord::Pending;
                (
                    ActionRequest::OpenMessage {
                        message_id: frozen.target.message_id.clone(),
                        claim: claims,
                    },
                    claims,
                )
            }
        };
        Some((
            RequestToken::new(frozen, RequestKind::Read { claims }),
            request,
        ))
    }

    /// Turn a confirmed action into the exact request it will send.
    ///
    /// Called at the yes, never at the drain. Resolving at drain time
    /// read whatever detail was open by then, so confirming on one row,
    /// leaving, and opening another built the first row's verb against
    /// the second row's target.
    fn resolve_action(
        &mut self,
        action: crate::detail::Action,
    ) -> Option<(
        crate::action_io::RequestToken,
        crate::action_io::ActionRequest,
    )> {
        use crate::action_io::{RequestKind, RequestToken};
        use crate::detail::Action;
        let detail = self.detail.as_mut()?;
        let frozen = detail.target().clone();
        let request = match (frozen.attempt, action) {
            (_, Action::Reply) => {
                let key = detail.draft_mut().key_for_send(next_client_key);
                crate::action_io::ActionRequest::Reply {
                    message_id: frozen.target.message_id.clone(),
                    body: detail.draft().text().to_string(),
                    client_key: key,
                }
            }
            (Some(attempt_id), Action::WithdrawNotification) => {
                crate::action_io::ActionRequest::WithdrawNotification {
                    attempt_id,
                    recipient: frozen.target.recipient,
                }
            }
            // The FROZEN attempt, never the row's current one. Between
            // the read and the yes an alarm can be cleared and requeued
            // under a new attempt, and the operator confirmed against the
            // evidence they were shown.
            (Some(attempt_id), Action::ClearAlarm) => {
                crate::action_io::ActionRequest::ClearAlarm { attempt_id }
            }
            (Some(attempt_id), Action::AttentionComplete) => {
                crate::action_io::ActionRequest::AttentionComplete { attempt_id }
            }
            (Some(attempt_id), Action::AttentionDiscard) => {
                crate::action_io::ActionRequest::AttentionDiscard { attempt_id }
            }
            // An attention verb with no attempt frozen never becomes a
            // request.
            (None, _) => return None,
        };
        Some((RequestToken::new(frozen, RequestKind::Act(action)), request))
    }

    /// Resolve an approved action and hold it for the loop.
    ///
    /// The resolution happens here, at the yes, so the request names the
    /// target the operator was looking at when they approved it.
    fn confirm_action(&mut self, action: crate::detail::Action) {
        // Never against a connection that is not acknowledged. Writing a
        // non-idempotent verb into a socket on its way down is exactly
        // the ambiguity the whole Uncertain path exists to report, and
        // refusing up front is cheaper than reporting it afterwards.
        if !self.refresh.may_mutate() {
            self.notice = Some("not connected: nothing was sent".into());
            return;
        }
        self.pending = self.resolve_action(action);
    }

    /// The confirmed request the loop should send. Taking it marks it in
    /// flight, so exactly one request is out and its answer can be
    /// matched back to it.
    pub fn take_pending(
        &mut self,
    ) -> Option<(
        crate::action_io::RequestToken,
        crate::action_io::ActionRequest,
    )> {
        if self.in_flight.is_some() {
            return None;
        }
        let (token, request) = self.pending.take()?;
        self.in_flight = Some(token.clone());
        Some((token, request))
    }

    /// The read a detail is owed, marked in flight.
    pub fn take_detail_read(
        &mut self,
    ) -> Option<(
        crate::action_io::RequestToken,
        crate::action_io::ActionRequest,
    )> {
        if !self.refresh.may_mutate() || self.in_flight.is_some() || !self.detail_read_owed() {
            return None;
        }
        let (token, request) = self.detail_read()?;
        self.in_flight = Some(token.clone());
        Some((token, request))
    }

    /// Tell an open detail whether its row is still listed. Never
    /// retargets it and never cancels a confirmation.
    fn resync_detail(&mut self) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        // Across the whole snapshot, not the current scope. A claim
        // moves its row out of Work, and looking only at what is visible
        // would mark the detail stale the moment its own claim landed.
        let current = self.queue.row_for(&detail.target().target).cloned();
        detail.observe_snapshot(current.as_ref());
    }

    /// Replace the queue with one authenticated snapshot.
    ///
    /// Whole replacement, never a merge: the daemon's answer is the
    /// state, and a client that patched rows would be a second read
    /// model free to disagree with it.
    pub fn apply_messages(&mut self, snapshot: &cyclops_proto::MessagesSnapshotResult) {
        self.queue
            .replace(crate::messages::rows_from_snapshot(snapshot));
        // The queue moved, so a detail's frozen target may no longer be
        // in it. Integration's gate owns finishing the request; this is
        // only about what the detail is looking at.
        self.resync_detail();
    }

    /// Apply a response only if it belongs to the current connection and
    /// covers every durable change already observed on the stream.
    pub fn apply_messages_response(
        &mut self,
        request: crate::messages::RefreshRequest,
        snapshot: &cyclops_proto::MessagesSnapshotResult,
    ) -> bool {
        if !self.refresh.finish_snapshot(request, snapshot) {
            return false;
        }
        self.apply_messages(snapshot);
        true
    }

    /// Return the next snapshot request, if one is owed. Asked once per
    /// loop turn.
    pub fn wants_messages(&mut self) -> Option<crate::messages::RefreshRequest> {
        self.refresh.begin()
    }

    /// The sidebar's rows, in pane-id order: stable across frames, so a
    /// row does not wander while the reader is aiming a click at it.
    pub fn roster(&self) -> impl Iterator<Item = &RosterRow> {
        self.roster.values()
    }

    pub fn roster_len(&self) -> usize {
        self.roster.len()
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.record.entries()
    }

    pub fn len(&self) -> usize {
        self.record.len()
    }

    pub fn is_empty(&self) -> bool {
        self.record.is_empty()
    }

    /// The register itself, for surfaces that need more than the count.
    pub fn attention(&self) -> &Attention {
        self.record.attention()
    }

    pub fn attention_count(&self) -> usize {
        self.record.attention_count()
    }

    pub fn admin_unread(&self) -> u64 {
        self.admin_unread
    }

    /// The eye as currently drawn (it may still be mid-tick).
    pub fn eye(&self) -> Eye {
        self.eye
    }

    /// Where the eye is headed given current attention.
    pub fn eye_target(&self) -> Eye {
        self.record.attention().eye()
    }

    /// Advance the eye one step toward its target. True means one more
    /// tick is wanted (the runtime schedules a single delayed redraw,
    /// never an interval).
    pub fn tick_eye(&mut self) -> bool {
        let target = self.eye_target();
        if self.eye != target {
            self.eye = self.eye.step_toward(target);
        }
        self.eye != self.eye_target()
    }

    /// One line replayed from the record: it goes on the screen and
    /// nowhere else ([`crate::stream::Record::replay`]).
    pub fn replay(&mut self, e: Entry) {
        self.observe_pane_name(&e);
        self.record.replay(e);
    }

    /// One live event from the daemon: it goes on the record AND moves the
    /// register ([`crate::stream::Record::live`]). This renderer's own
    /// navigation state (the sidebar row and the focus-jump map) moves
    /// on the same live edge, and only live: a replayed line is old news
    /// and must not restart anyone's clock.
    ///
    /// Returns the clearance line when the transition ended something that
    /// needed a human. It is already on the record; the caller gets it
    /// because `--plain` prints line by line and has no frame to reconcile
    /// (plain.rs), so a line it never sees is a line it never prints.
    pub fn live(&mut self, e: Entry) -> Option<Entry> {
        self.observe_pane_name(&e);
        match &e.kind {
            EntryKind::State {
                target,
                session_idx,
                pane_id: Some(p),
                state,
                ..
            } => self.roster_observe(*session_idx, p, target, *state),
            // Remove only the route this watcher lost. A transfer can
            // publish the destination state before this source edge, and
            // both routes carry the same tmux pane id.
            EntryKind::PaneGone {
                session_idx,
                pane_id,
                ..
            } => {
                let route = PaneRoute::new(*session_idx, pane_id);
                self.panes.retain(|_, current| current != &route);
                self.roster.remove(&route);
            }
            _ => {}
        }
        self.record.live(e)
    }

    /// Harvest the label -> pane map from a State entry, replayed or live
    /// alike: a replayed line may still be the freshest naming the jump
    /// has. The seed lands after the replayed tail and live events after
    /// the seed, so the newest naming always wins.
    fn observe_pane_name(&mut self, e: &Entry) {
        if let EntryKind::State {
            target,
            session_idx,
            pane_id: Some(p),
            ..
        } = &e.kind
        {
            self.panes
                .insert(target.clone(), PaneRoute::new(*session_idx, p));
        }
    }

    /// One live state event moving a sidebar row.
    ///
    /// The elapsed clock restarts only when the STATE changed; the same
    /// state re-observed (the daemon re-emits on unrelated recomputes) is
    /// confirmation, not a transition, and a clock that reset on it would
    /// show every long-running agent as seconds old.
    fn roster_observe(
        &mut self,
        session_idx: usize,
        pane_id: &str,
        name: &str,
        state: cyclops_proto::AgentState,
    ) {
        let route = PaneRoute::new(session_idx, pane_id);
        match self.roster.get_mut(&route) {
            Some(row) => {
                row.name = name.to_string();
                if row.state != state {
                    row.state = state;
                    row.since = Since::Observed(std::time::Instant::now());
                }
            }
            None => {
                // A pane the seed never saw: it appeared after startup.
                self.roster.insert(
                    route,
                    RosterRow {
                        session_idx,
                        name: name.to_string(),
                        pane_id: pane_id.to_string(),
                        state,
                        manifest: None,
                        since: Since::Observed(std::time::Instant::now()),
                    },
                );
            }
        }
    }

    /// Startup reconciliation from the daemon's one status answer.
    ///
    /// This renderer's own navigation state first: the sidebar roster (1)
    /// and the focus-jump map (2) are seeded fresh from the same answer,
    /// for the same reason the register is replaced whole rather than
    /// merged (anything the answer does not list is gone). Replacing the
    /// register and writing the lines and clearances every count on
    /// screen needs behind it (3) is [`crate::stream::Record::seed`]'s job,
    /// so a workspace panel reading the same daemon answer reconciles
    /// identically.
    ///
    /// Returns the entries the caller should ingest (via [`App::replay`]),
    /// because in `--plain` they must also print.
    ///
    /// Event-driven only: called once at startup and never on a timer.
    /// After that the register moves on live events alone ([`App::live`]).
    pub fn seed_status(&mut self, seed: StatusSeed) -> Vec<Entry> {
        self.admin_unread = seed.admin_unread;
        self.filter_routes = seed.mailbox_routes.clone();
        // 1. The sidebar's roster is the answer's pane list, replaced
        //    whole for the same reason the register is: anything the
        //    answer does not list is gone. The daemon's own elapsed
        //    anchors each clock; from here live events move it.
        let seeded_at = std::time::Instant::now();
        self.roster = seed
            .roster
            .iter()
            .map(|r| {
                (
                    PaneRoute::new(r.session_idx, &r.pane_id),
                    RosterRow {
                        session_idx: r.session_idx,
                        name: r.name.clone(),
                        pane_id: r.pane_id.clone(),
                        state: r.state,
                        manifest: r.manifest.clone(),
                        since: match r.state_ms {
                            Some(ms) => Since::Seeded {
                                state_ms: ms,
                                at: seeded_at,
                            },
                            None => Since::Unknown,
                        },
                    },
                )
            })
            .collect();
        // 2. Refresh the jump map, which outlives the roster: a label the
        //    answer still names points at the pane the answer named.
        for p in &seed.roster {
            self.panes
                .insert(p.name.clone(), PaneRoute::new(p.session_idx, &p.pane_id));
        }
        // 3. The register and the backlog's lines are the model's job.
        let routes = seed
            .roster
            .iter()
            .map(|pane| (pane.pane_id.clone(), pane.session_idx))
            .collect();
        let recipients: HashMap<String, _> = seed
            .mailbox_routes
            .iter()
            .flat_map(|route| {
                let recipient = route.recipient();
                let mut keys = vec![(route.label().to_string(), recipient)];
                if let Some(pane_id) = recipient.pane_id() {
                    keys.push((pane_id.to_string(), recipient));
                }
                keys
            })
            .collect();
        self.record
            .seed_routed(&seed.panes, &seed.open, &routes, &recipients)
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
        self.record.attention_items()
    }

    /// Counted items with no line in the current view.
    ///
    /// The header may never show a number the reader cannot reach, so
    /// whatever this returns has to be said on the frame. Two things put
    /// an item here: a filter that hides its line, and eviction from the
    /// 10k ring. The startup reconciliation writes a line for everything
    /// else, so nothing else can.
    ///
    /// One walk of the view, not one per item: [`crate::stream::Record::
    /// unreachable`] does it in a single pass against `visible`, which the
    /// caller already built once to draw the frame.
    ///
    /// Measured against the naive scan over a full 10,000-entry firehose:
    /// 7.0ms at 50 items, 13.3ms at 100, 15.8ms at 120, 19.6ms at 150,
    /// 51ms at 400. The 16ms frame budget goes at roughly 125 items, not
    /// at the 400 the perf list happens to start failing on, and a quota
    /// weekend leaves hundreds. The admin view stays cheap either way
    /// (2.5ms at 400) because it filters the ring down first, and it is
    /// the default view, which is why the firehose is what this is sized
    /// for. Numbers from src/cyclops-ui/tests/perf.rs on the dev
    /// machine; the test asserts the budget rather than these times.
    pub fn unreachable(&self, visible: &[&Entry]) -> Vec<AttentionItem> {
        self.record.unreachable(visible)
    }

    /// Entries the current view and filter admit, oldest first.
    pub fn visible(&self) -> Vec<&Entry> {
        self.record
            .entries()
            .filter(|e| self.admits_in_view(e))
            .filter(|e| self.filter.matches(e))
            .collect()
    }

    /// Does the CURRENT view admit this line? The firehose admits
    /// everything; the admin stream asks [`crate::stream::Record::admits`].
    pub fn admits_in_view(&self, e: &Entry) -> bool {
        self.view == View::Firehose
            || (!self.filter.is_empty() && matches!(&e.kind, EntryKind::Msg { .. }))
            || self.record.admits(e)
    }

    /// Handle one key. Returns a command for the runtime when the key
    /// asks for more than state change.
    /// Keys while a detail is open.
    ///
    /// Actions are chosen from the numbered list the footer shows, never
    /// from a bare letter: a destructive verb should not be one keystroke
    /// away from a reader who is scrolling. Anything that mutates then
    /// asks for `y` against a sentence naming the target.
    fn handle_detail_key(&mut self, key: Key) -> Option<Command> {
        // The composer comes FIRST, before the size and busy guards.
        //
        // Both of those refuse `y` and digits, and both can arm without a
        // keystroke: a background snapshot sets needs_reload, which puts a
        // read in flight, and a pane can be resized under an open
        // composer. Running them above this branch meant a `y` or a digit
        // typed into a reply body was swallowed by a guard meant for
        // actions, and the body went out mangled with nothing on screen
        // to show it. While a reply is being written the keyboard belongs
        // to it, which is what this ordering finally makes true.
        let composing = self
            .detail
            .as_ref()
            .is_some_and(|detail| detail.is_composing());
        if composing {
            if key == Key::CtrlD && !self.refresh.may_mutate() {
                if self.notice.is_none() {
                    self.notice = Some("not connected: nothing was sent".into());
                }
                return None;
            }
            // Sending is the one thing a too-small frame still refuses,
            // because a frame that cannot draw the draft cannot show what
            // is about to be sent. Typing and cancelling stay live.
            if key == Key::CtrlD
                && !crate::detail::can_show_actions(self.last_size.0, self.detail_action_height())
            {
                self.notice = Some("open a larger window to send this".into());
                return None;
            }
            if key == Key::CtrlD && (self.in_flight.is_some() || self.pending.is_some()) {
                self.notice = Some("waiting for the last action to answer".into());
                return None;
            }
            let mut send_reply = false;
            let mut draft_full = false;
            let detail = self.detail.as_mut()?;
            match key {
                Key::CtrlC => return Some(Command::Quit),
                Key::Esc => detail.end_compose(),
                Key::Backspace => detail.draft_mut().backspace(),
                // Enter is a newline, never a send. A pasted newline and a
                // typed one are the same byte by the time they arrive, so
                // a composer that sent on Enter would send whatever came
                // before the first line break of a paste.
                Key::Enter => draft_full = !detail.draft_mut().push('\n'),
                Key::CtrlD => {
                    // request() again, at the moment of sending. It is the
                    // staleness gate, and the row can go stale while the
                    // reader types. It also stamps Acting here, where
                    // something really is about to be on the wire.
                    if !detail.draft().is_empty() {
                        match detail.request(crate::detail::Action::Reply) {
                            crate::detail::Request::Perform(_) => {
                                detail.end_compose();
                                send_reply = true;
                            }
                            _ => {
                                detail.end_compose();
                                self.notice = Some("this row is no longer available".into());
                            }
                        }
                    }
                }
                Key::Char(c) => draft_full = !detail.draft_mut().push(c),
                _ => {}
            }
            if draft_full {
                self.notice = Some(format!(
                    "reply is limited to {} KiB",
                    crate::detail::DRAFT_MAX_BYTES / 1024
                ));
            }
            if send_reply {
                self.confirm_action(crate::detail::Action::Reply);
            }
            return None;
        }
        if key == Key::Char('?') {
            self.notice = Some("detail keys are shown below; esc returns to the queue".into());
            return None;
        }
        // Read before the detail is borrowed: the question is about the
        // frame, not about the detail.
        let can_act =
            crate::detail::can_show_actions(self.last_size.0, self.detail_action_height());
        if !self.refresh.may_mutate() && matches!(key, Key::Char('y') | Key::Char('1'..='9')) {
            if self.notice.is_none() {
                self.notice = Some("not connected: nothing was sent".into());
            }
            return None;
        }
        if !can_act && matches!(key, Key::Char('y') | Key::Char('1'..='9')) {
            self.notice = Some("open a larger window to act on this".into());
            return None;
        }
        // A request is out. Anything that could start a second one, or
        // change what the first was about, waits for its answer.
        // Scrolling and leaving stay live: neither can retarget, and a
        // response arriving after the reader left is dropped on its
        // token anyway.
        let busy = self.in_flight.is_some() || self.pending.is_some();
        if busy && matches!(key, Key::Char('y') | Key::Char('1'..='9')) {
            self.notice = Some("waiting for the last action to answer".into());
            return None;
        }
        let detail = self.detail.as_mut()?;
        match key {
            Key::Char('q') | Key::CtrlC => return Some(Command::Quit),
            Key::Esc => {
                if detail.escape() == crate::detail::Back::Closed {
                    self.detail = None;
                    // Nothing is waiting on those answers any more. The
                    // requests may still land, and their tokens no
                    // longer match, so they are dropped when they do.
                    // Forgetting them here is what frees the reader to
                    // open something else instead of waiting out a slow
                    // read they have already left.
                    self.in_flight = None;
                    self.pending = None;
                }
            }
            Key::Char('y') => {
                if let Some(action) = detail.confirm() {
                    self.confirm_action(action);
                }
            }
            Key::Char(c) if c.is_ascii_digit() && c != '0' => {
                let index = (c as u8 - b'1') as usize;
                let allowed = detail.allowed();
                if let Some(&action) = allowed.get(index) {
                    // Every verb goes through request(), reply included.
                    // request() is where "a stale detail offers nothing at
                    // all" is enforced, so the one action that skipped it
                    // was the one action a stale detail could still
                    // perform: the header read "(no longer listed)" and
                    // the footer "no actions available" while the keyboard
                    // still sent a message.
                    match detail.request(action) {
                        // Reply opens the composer rather than sending an
                        // empty body. It is the only Perform that is not a
                        // request to the daemon yet.
                        crate::detail::Request::Perform(crate::detail::Action::Reply) => {
                            detail.begin_reply()
                        }
                        crate::detail::Request::Perform(action) => self.confirm_action(action),
                        // The stage now holds the confirmation; the footer
                        // draws the sentence and waits for a yes.
                        crate::detail::Request::Confirm(_) => {}
                        crate::detail::Request::Refused(why) => self.notice = Some(why.to_string()),
                    }
                }
            }
            // Ask again, by hand. A failed reload stops being owed so the
            // loop cannot spin on it, which means nothing retries on its
            // own and the evidence stays marked stale until this.
            Key::Char('r') => detail.retry_reload(),
            // A long body scrolls under the same keys the list uses.
            Key::Up | Key::Char('k') | Key::WheelUp => detail.scroll_by(-1),
            Key::Down | Key::Char('j') | Key::WheelDown => detail.scroll_by(1),
            _ => {}
        }
        None
    }

    /// Is a reply composer open right now? The only place pasted bytes
    /// are allowed to land.
    fn composing_now(&self) -> bool {
        self.view == View::Messages
            && self
                .detail
                .as_ref()
                .is_some_and(|detail| detail.is_composing())
    }

    /// Height available to evidence after the Messages status row.
    fn detail_action_height(&self) -> usize {
        let status_visible = !self.refresh.may_mutate() || self.notice_text().is_some();
        self.last_size.1.saturating_sub(usize::from(status_visible))
    }

    /// Put one pasted character into the open draft, or drop it.
    fn paste_into_draft(&mut self, ch: Option<char>) {
        if !self.composing_now() {
            return;
        }
        if let (Some(detail), Some(ch)) = (self.detail.as_mut(), ch) {
            if !detail.draft_mut().push(ch) {
                self.notice = Some(format!(
                    "reply is limited to {} KiB",
                    crate::detail::DRAFT_MAX_BYTES / 1024
                ));
            }
        }
    }

    pub fn handle_key(&mut self, key: Key) -> Option<Command> {
        if key == Key::PasteRejected {
            self.pasting = false;
            self.notice = Some("paste refused: exceeds the UI ingress bound".into());
            return None;
        }
        // Bracketed paste is settled HERE, above every route, because
        // every route below reads keys as commands. A guard further down
        // only protected the detail: a paste landing on the queue, on a
        // filter input line, or on either stream view still ran its bytes
        // as keystrokes, so a pasted q quit and pasted digits acted.
        //
        // Inside a paste exactly one thing is admitted, an active reply
        // draft. Everywhere else the bytes are dropped.
        if self.pasting {
            match key {
                Key::PasteEnd => self.pasting = false,
                Key::Char(c) => self.paste_into_draft(Some(c)),
                Key::Enter => self.paste_into_draft(Some('\n')),
                // Nothing else. No key inside a paste ends the paste,
                // acts, or quits, including Esc and Ctrl-C: a clipboard
                // can legally carry either, and any byte that could
                // return the UI to command mode early would let the bytes
                // behind it run as commands. The decoder guarantees a
                // PasteEnd for every PasteStart it emits, so this cannot
                // strand the keyboard.
                _ => {}
            }
            return None;
        }
        if key == Key::PasteStart {
            self.pasting = true;
            if !self.composing_now() {
                self.notice = Some("paste ignored: open the reply first".into());
            }
            return None;
        }
        // An open input line captures everything except control keys.
        if self.input.is_some() {
            return self.handle_input_key(key);
        }
        // Reconnect is explicit and single-flight. A reply draft still
        // owns every printable character, including uppercase R.
        if key == Key::Char('R') && !self.composing_now() && self.refresh.reconnecting() {
            self.notice = None;
            self.reconnect_owed = true;
            return Some(Command::Reconnect);
        }
        // A detail owns the keyboard while it is open, so a stream key
        // cannot move something underneath it.
        if self.view == View::Messages && self.detail.is_some() {
            return self.handle_detail_key(key);
        }
        // With no detail open, Messages navigates the queue. Without
        // this the arrows moved the stream's record instead, so only the
        // row the snapshot seeded was ever reachable and three of the
        // four scopes could not be entered at all.
        if self.view == View::Messages {
            match key {
                Key::Char('q') | Key::CtrlC => return Some(Command::Quit),
                Key::Char('?') => {
                    self.overlay = !self.overlay;
                    return None;
                }
                Key::Esc => {
                    self.overlay = false;
                    self.notice = None;
                    return None;
                }
                Key::Tab => {
                    self.overlay = false;
                    self.view = View::Admin;
                    self.repin();
                    return None;
                }
                _ if self.overlay => return None,
                Key::Up | Key::Char('k') | Key::WheelUp => {
                    self.queue.select_previous();
                    return None;
                }
                Key::Down | Key::Char('j') | Key::WheelDown => {
                    self.queue.select_next();
                    return None;
                }
                // One key, forward through the four scopes. The current
                // one is already named in the queue header, so there is
                // nothing to remember.
                Key::Char('s') => {
                    let order = crate::queue::Scope::ORDER;
                    let at = order
                        .iter()
                        .position(|s| *s == self.queue.scope())
                        .unwrap_or(0);
                    self.queue.set_scope(order[(at + 1) % order.len()]);
                    return None;
                }
                Key::Enter => {
                    if !self.refresh.may_mutate() {
                        return None;
                    }
                    self.notice = None;
                    if self.open_detail().is_none() {
                        self.notice = Some(if self.queue.is_empty() {
                            "nothing to open".into()
                        } else {
                            "message changed; select a row".into()
                        });
                    }
                    return None;
                }
                _ => return None,
            }
        }
        match key {
            Key::Char('q') | Key::CtrlC => return Some(Command::Quit),
            Key::Char('?') => self.overlay = !self.overlay,
            Key::Esc => {
                self.overlay = false;
                self.notice = None;
            }
            Key::Tab => {
                self.view = match self.view {
                    View::Admin => View::Firehose,
                    View::Firehose => View::Messages,
                    View::Messages => View::Admin,
                };
                // The other view is a different list; stale anchors would
                // land anywhere. Rejoin the tail.
                self.repin();
            }
            Key::Char('c') => {
                self.density = match self.density {
                    Density::Comfortable => Density::Compact,
                    Density::Compact => Density::Comfortable,
                };
            }
            Key::Char('a') => self.show_roster = !self.show_roster,
            // A click means what the clicked cell means: a sidebar agent
            // jumps focus (the act the sidebar exists for), a stream entry
            // becomes the selection, dead space does nothing. The frame
            // wrote row_targets as it laid the screen out, so the mouse
            // and the eye agree about what is where; x picks the half.
            Key::Click { x, y } => {
                self.notice = None;
                let target = self.row_targets.get(y as usize).map(|(side, stream)| {
                    if (x as usize) < self.sidebar_w {
                        side.clone()
                    } else {
                        stream.clone()
                    }
                });
                match target {
                    Some(RowTarget::Agent(pane_id)) => {
                        return Some(Command::Focus(pane_id));
                    }
                    Some(RowTarget::Entry(uid)) => {
                        self.selected = Some(uid);
                        self.pinned = false;
                    }
                    _ => {}
                }
            }
            // A wheel notch is three rows, matching every terminal list
            // there is; the first notch unpins like ↑ does.
            Key::WheelUp => {
                for _ in 0..3 {
                    self.select_prev();
                }
            }
            Key::WheelDown => {
                for _ in 0..3 {
                    self.select_next();
                }
            }
            Key::Char('w') => self.open_input(Which::With),
            Key::Char('f') => self.open_input(Which::From),
            Key::Char('t') => self.open_input(Which::To),
            Key::Up | Key::Char('k') => self.select_prev(),
            Key::Down | Key::Char('j') => self.select_next(),
            Key::End | Key::Char('G') => self.repin(),
            Key::Enter => {
                self.notice = None;
                if let Some(target) = self.selected_focus_target() {
                    return Some(Command::Focus(target));
                }
                // Resolution may have left a more specific notice already.
                if self.notice.is_none() {
                    self.notice = Some("nothing to jump to on this line".into());
                }
            }
            _ => {}
        }
        None
    }

    fn handle_input_key(&mut self, key: Key) -> Option<Command> {
        match key {
            Key::Enter => {
                let input = self.input.as_ref().expect("input line open");
                let which = input.which;
                let asked = input.buf.trim().to_string();
                let value = if asked.is_empty() {
                    None
                } else {
                    match self.resolve_filter(&asked) {
                        Ok(value) => Some(value),
                        Err(why) => {
                            self.notice = Some(why);
                            return None;
                        }
                    }
                };
                match which {
                    // with excludes from/to and vice versa, mirroring the
                    // history flags.
                    Which::With => {
                        self.filter.with = value;
                        if self.filter.with.is_some() {
                            self.filter.from = None;
                            self.filter.to = None;
                        }
                    }
                    Which::From => {
                        self.filter.from = value;
                        self.filter.with = None;
                    }
                    Which::To => {
                        self.filter.to = value;
                        self.filter.with = None;
                    }
                }
                self.input = None;
                self.repin();
            }
            Key::Esc => self.input = None,
            Key::Backspace => {
                let input = self.input.as_mut().expect("input line open");
                input.buf.pop();
            }
            Key::Char(c) if !c.is_control() => {
                self.input.as_mut().expect("input line open").buf.push(c)
            }
            Key::CtrlC => return Some(Command::Quit),
            _ => {}
        }
        None
    }

    fn resolve_filter(&self, asked: &str) -> Result<EndpointFilter, String> {
        let matches: Vec<_> = self
            .filter_routes
            .iter()
            .filter(|route| route.label() == asked || route.recipient().to_string() == asked)
            .cloned()
            .collect();
        match matches.as_slice() {
            [route] => Ok(route.clone()),
            [] if self.filter_routes.is_empty() => {
                Err("filter routes are not loaded yet; try again after status arrives".into())
            }
            [] => Err(format!("unknown mailbox endpoint '{asked}'")),
            _ => Err(format!(
                "mailbox label '{asked}' is ambiguous; use an exact recipient key"
            )),
        }
    }

    /// Open a filter's input line, pre-filled with its current value so a
    /// set filter is edited rather than retyped. Enter on the pre-filled
    /// line re-applies it; emptying the line first is what clears it,
    /// which is why the attention band says so.
    fn open_input(&mut self, which: Which) {
        self.input = Some(InputLine {
            which,
            buf: which.value(&self.filter).unwrap_or_default().to_string(),
        });
    }

    /// Back to the tail: pinned, selection follows arrivals.
    pub fn repin(&mut self) {
        self.pinned = true;
        self.selected = None;
        self.top = None;
    }

    fn select_prev(&mut self) {
        let uids: Vec<u64> = self.visible().iter().map(|e| e.uid).collect();
        if uids.is_empty() {
            return;
        }
        let pos = match self.selected {
            None => uids.len() - 1,
            Some(uid) => uids
                .iter()
                .position(|u| *u == uid)
                .unwrap_or(uids.len() - 1),
        };
        self.selected = Some(uids[pos.saturating_sub(1)]);
        self.pinned = false;
    }

    fn select_next(&mut self) {
        if self.pinned {
            return;
        }
        let uids: Vec<u64> = self.visible().iter().map(|e| e.uid).collect();
        let pos = self
            .selected
            .and_then(|uid| uids.iter().position(|u| *u == uid));
        match pos {
            Some(p) if p + 1 < uids.len() => self.selected = Some(uids[p + 1]),
            _ => self.repin(),
        }
    }

    /// The pane id behind the selected entry: a literal pane id passes
    /// through, a label resolves via the harvested map.
    fn selected_focus_target(&mut self) -> Option<String> {
        let name: Option<String> = {
            let list = self.visible();
            let entry = match self.selected {
                Some(uid) => list.iter().find(|e| e.uid == uid).copied(),
                None => list.last().copied(),
            };
            entry.and_then(Entry::focus_target).map(String::from)
        };
        let name = name?;
        if name.starts_with('%') {
            return Some(name);
        }
        match self.panes.get(&name) {
            Some(route) => Some(route.pane_id.clone()),
            None => {
                self.notice = Some(format!("no pane known for {name}"));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{EntryKind, RosterSeed, RING_CAP};
    use cyclops_proto::{AgentState, DeliveryState, OpenDelivery, PaneSnapshot};

    #[test]
    fn a_rejected_paste_is_visible_and_leaves_command_mode() {
        let mut app = App::new(Theme::none(), View::Admin, Filter::default());
        app.pasting = true;
        assert_eq!(app.handle_key(Key::PasteRejected), None);
        assert!(!app.pasting);
        assert_eq!(
            app.notice.as_deref(),
            Some("paste refused: exceeds the UI ingress bound")
        );
    }

    fn endpoint(label: &str) -> EndpointFilter {
        EndpointFilter::new(
            "admin:00000000-0000-0000-0000-000000000001"
                .parse()
                .unwrap(),
            label,
        )
    }

    fn msg(from: &str, to: &[&str]) -> Entry {
        Entry {
            uid: 0,
            ts: 1000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Msg {
                from: from.into(),
                to: to.iter().map(|t| t.to_string()).collect(),
                endpoints: None,
                subject: "s".into(),
                body: None,
                fyi: false,
            },
        }
    }

    /// A state entry for `target` on its own pane. Two agents are two
    /// panes, which is what the attention map keys on.
    fn state(target: &str, s: AgentState) -> Entry {
        state_on(target, Some(pane_of(target)), s)
    }

    fn pane_of(target: &str) -> &'static str {
        match target {
            "implementer" => "%2",
            "ghost" => "%3",
            _ => "%1",
        }
    }

    /// A state entry naming its pane explicitly. The daemon emits the fused
    /// state under the pane id before adoption and under the label after,
    /// so the two names for one pane have to be exercised together.
    fn state_on(target: &str, pane_id: Option<&str>, s: AgentState) -> Entry {
        Entry {
            uid: 0,
            ts: 1000,
            seq: None,
            id: Some("e-1".into()),
            kind: EntryKind::State {
                target: target.into(),
                recipient: None,
                session_idx: 0,
                pane_id: pane_id.map(String::from),
                state: s,
            },
        }
    }

    /// One delivery transition of message `id` to `to`.
    fn delivery(id: &str, to: &str, s: DeliveryState) -> Entry {
        Entry {
            uid: 0,
            ts: 1000,
            seq: None,
            id: Some(id.into()),
            kind: EntryKind::Delivery {
                to: to.into(),
                recipient: None,
                state: s,
                cause: None,
            },
        }
    }

    fn gate(to: &str, action: &str, cause: &str) -> Entry {
        Entry {
            uid: 0,
            ts: 1000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Gate {
                to: to.into(),
                recipient: None,
                action: action.into(),
                detail: Some(cause.into()),
            },
        }
    }

    fn app() -> App {
        App::new(Theme::none(), View::Admin, Filter::default())
    }

    /// One status answer: the panes the daemon currently sees and the
    /// deliveries its fold still counts.
    fn seed(panes: &[(&str, &str, AgentState)], open: Vec<OpenDelivery>) -> StatusSeed {
        StatusSeed {
            watched: vec!["main".into()],
            panes: panes
                .iter()
                .map(|(name, pane_id, state)| PaneSnapshot {
                    pane_id: (*pane_id).into(),
                    name: (*name).into(),
                    state: *state,
                })
                .collect(),
            open,
            admin_unread: 0,
            mailbox_routes: Vec::new(),
            roster: panes
                .iter()
                .map(|(name, pane_id, state)| RosterSeed {
                    session_idx: 0,
                    pane_id: (*pane_id).into(),
                    name: (*name).into(),
                    state: *state,
                    manifest: None,
                    state_ms: Some(5_000),
                })
                .collect(),
        }
    }

    fn open(id: &str, to: &str, state: DeliveryState) -> OpenDelivery {
        OpenDelivery {
            id: id.into(),
            to: to.into(),
            recipient: None,
            state,
            ts: 1000,
            cause: None,
        }
    }

    #[test]
    fn ring_caps_and_uids_stay_unique() {
        let mut a = app();
        for _ in 0..(RING_CAP + 10) {
            a.live(msg("codex", &["reviewer"]));
        }
        assert_eq!(a.len(), RING_CAP);
        // The oldest were evicted: the first uid in the ring is 11.
        assert_eq!(a.entries().next().unwrap().uid, 11);
    }

    #[test]
    fn admin_view_hides_agent_chatter_firehose_shows_all() {
        let mut a = app();
        a.live(msg("codex", &["reviewer"]));
        a.live(msg("codex", &["admin"]));
        a.live(state("reviewer", AgentState::Working));
        assert_eq!(a.visible().len(), 1);
        a.view = View::Firehose;
        assert_eq!(a.visible().len(), 3);
    }

    #[test]
    fn eye_follows_attention_with_one_tick_between() {
        let mut a = app();
        assert_eq!(a.eye(), Eye::Closed);
        // Two blocked agents: target open, one intermediate tick.
        a.live(state("reviewer", AgentState::BlockedPermission));
        a.live(state("implementer", AgentState::BlockedQuota));
        assert_eq!(a.eye_target(), Eye::Open);
        assert!(a.tick_eye(), "one more tick wanted");
        assert_eq!(a.eye(), Eye::Opening);
        assert!(!a.tick_eye());
        assert_eq!(a.eye(), Eye::Open);
        // Both clear: back through opening to closed.
        a.live(state("reviewer", AgentState::Idle));
        a.live(state("implementer", AgentState::Idle));
        assert_eq!(a.eye_target(), Eye::Closed);
        assert!(a.tick_eye());
        assert_eq!(a.eye(), Eye::Opening);
        assert!(!a.tick_eye());
        assert_eq!(a.eye(), Eye::Closed);
    }

    #[test]
    fn attention_counts_blocked_agents_and_needy_deliveries() {
        let mut a = app();
        a.live(state("reviewer", AgentState::BlockedPermission));
        assert_eq!(a.attention_count(), 1);
        a.live(delivery(
            "m-1",
            "implementer",
            DeliveryState::AttentionRequired,
        ));
        assert_eq!(a.attention_count(), 2);
        // Supersessions clear their item: the same agent's next state,
        // and the same delivery's next transition.
        a.live(state("reviewer", AgentState::Idle));
        a.live(delivery("m-1", "implementer", DeliveryState::Queued));
        assert_eq!(a.attention_count(), 0);
    }

    #[test]
    fn a_later_message_never_clears_another_messages_attention() {
        let mut a = app();
        // m-1 to reviewer ends needing a human. Nothing auto-retries it.
        a.live(delivery(
            "m-1",
            "reviewer",
            DeliveryState::AttentionRequired,
        ));
        assert_eq!(a.attention_count(), 1);
        // m-2 to the same reviewer sails through. m-1 is still unresolved,
        // so the count holds and the eye stays open on it.
        a.live(delivery("m-2", "reviewer", DeliveryState::Queued));
        a.live(delivery(
            "m-2",
            "reviewer",
            DeliveryState::DeliveredVerified,
        ));
        assert_eq!(a.attention_count(), 1, "m-2 closed m-1's attention item");
        assert_eq!(a.eye_target(), Eye::Opening);
        // Settle the animation: the drawn eye is what the header shows,
        // and it must not read "All calm" over an unresolved delivery.
        while a.tick_eye() {}
        assert_eq!(a.eye(), Eye::Opening);
        // A second unresolved delivery to the same recipient counts twice.
        a.live(delivery(
            "m-3",
            "reviewer",
            DeliveryState::ParkedBlockedQuota,
        ));
        assert_eq!(a.attention_count(), 2);
        assert_eq!(a.eye_target(), Eye::Open);
        // Only m-1's own requeue clears m-1; m-3 keeps the eye open.
        a.live(delivery("m-1", "reviewer", DeliveryState::Queued));
        assert_eq!(a.attention_count(), 1);
        assert_eq!(a.eye_target(), Eye::Opening);
        a.live(delivery("m-3", "reviewer", DeliveryState::Queued));
        assert_eq!(a.attention_count(), 0);
        assert_eq!(a.eye_target(), Eye::Closed);
    }

    /// A fresh CLI pane sitting on a trust dialog is the ordinary first-run
    /// case: the daemon emits its fused state under the pane id until the
    /// pane is adopted, then under the label. Keying the emitted name
    /// verbatim left the bootstrap item under "%1" while every later event
    /// answered under "reviewer", so nothing could clear it and the count
    /// never returned to zero for the life of the process.
    #[test]
    fn a_pane_that_blocks_before_adoption_clears_after_it() {
        let mut a = app();
        a.live(state_on("%1", Some("%1"), AgentState::BlockedPermission));
        assert_eq!(a.attention_count(), 1);
        // Same pane, now labeled. Its next state answers for the item.
        a.live(state_on("reviewer", Some("%1"), AgentState::Idle));
        assert_eq!(
            a.attention_count(),
            0,
            "the pre-adoption item outlived the pane's own next state"
        );
        assert_eq!(a.eye_target(), Eye::Closed);
        // The seed agrees: it carries the pane id, so a status answer for
        // an adopted pane clears an item seeded before the label existed.
        a.live(state_on("%2", Some("%2"), AgentState::BlockedModal));
        assert_eq!(a.attention_count(), 1);
        a.seed_status(seed(&[("implementer", "%2", AgentState::Idle)], Vec::new()));
        assert_eq!(a.attention_count(), 0, "the seed missed the pane id");
        // A pane-less state line (an older daemon, a hand-written record)
        // still lands: the target is the only name it has.
        a.live(state_on("ghost", None, AgentState::BlockedQuota));
        assert_eq!(a.attention_count(), 1);
        a.live(state_on("ghost", None, AgentState::Idle));
        assert_eq!(a.attention_count(), 0);
    }

    #[test]
    fn id_less_deliveries_still_key_by_recipient() {
        // A daemon that omits the id (or a hand-written ledger line) must
        // still land an attention item rather than vanish.
        let mut a = app();
        let mut e = delivery("m-1", "reviewer", DeliveryState::AttentionRequired);
        e.id = None;
        a.live(e);
        assert_eq!(a.attention_count(), 1);
        let mut e = delivery("m-1", "reviewer", DeliveryState::Queued);
        e.id = None;
        a.live(e);
        assert_eq!(a.attention_count(), 0);
    }

    /// Every number the header shows must have a line behind it, and only
    /// one. The seed writes a line for each item the loaded stream does
    /// not already account for, and stays quiet where it does.
    #[test]
    fn the_seed_puts_exactly_one_line_behind_every_count() {
        let mut a = app();
        a.view = View::Firehose;
        // The stream already says reviewer is blocked, and says nothing at
        // all about implementer.
        a.live(state("reviewer", AgentState::BlockedPermission));
        // Stale line for a third pane: the seed disagrees and must say so.
        a.live(state_on("codex", Some("%4"), AgentState::Working));
        let lines = a.seed_status(seed(
            &[
                ("reviewer", "%1", AgentState::BlockedPermission),
                ("implementer", "%2", AgentState::BlockedQuota),
                ("codex", "%4", AgentState::BlockedModal),
                // Nothing to act on, so nothing to explain.
                ("docs", "%5", AgentState::Idle),
            ],
            Vec::new(),
        ));
        let named: Vec<&str> = lines
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::State { target, .. } => Some(target.as_str()),
                _ => None,
            })
            .collect();
        // Register order, which is name order: the written lines all carry
        // the same reading time, so the daemon's pane order would be the
        // only thing deciding it otherwise.
        assert_eq!(named, vec!["codex", "implementer"]);
        for e in lines {
            a.replay(e);
        }
        assert_eq!(a.attention_count(), 3);
        assert_eq!(
            a.attention_items(),
            vec![
                "codex  ⚠ blocked_modal",
                "implementer  ⊘ blocked_quota",
                "reviewer  ⚠ blocked_permission",
            ]
        );
        // Every one of the three is reachable: the seed wrote the two the
        // stream was missing, and the third was already there.
        assert!(a.unreachable(&a.visible()).is_empty());
    }

    /// The count answers to the daemon, not to how much tail was replayed.
    /// Both halves: a blocked pane and an open delivery seen only in
    /// history must not raise the eye, and a pane the answer no longer
    /// lists must stop raising it.
    #[test]
    fn replayed_history_never_moves_the_count() {
        let mut a = app();
        a.replay(state("reviewer", AgentState::BlockedPermission));
        a.replay(delivery(
            "m-park",
            "implementer",
            DeliveryState::ParkedBlockedQuota,
        ));
        assert_eq!(a.attention_count(), 0, "the tail decided the count");
        assert_eq!(a.len(), 2, "the tail must still be on screen");

        // The daemon's answer is the count, and it holds the same two.
        a.seed_status(seed(
            &[("reviewer", "%1", AgentState::BlockedPermission)],
            vec![open(
                "m-park",
                "implementer",
                DeliveryState::ParkedBlockedQuota,
            )],
        ));
        assert_eq!(a.attention_count(), 2);

        // The pane is gone from the next answer and the park was requeued:
        // nothing counts, and no stale line can hold either item open.
        a.seed_status(seed(&[], Vec::new()));
        assert_eq!(a.attention_count(), 0, "a vanished pane still counted");
        assert_eq!(a.eye_target(), Eye::Closed);
    }

    /// A counted item whose line the filter hides is still reachable: the
    /// frame has to be able to name it, in any view, under any filter.
    #[test]
    fn a_filtered_out_item_reports_as_unreachable() {
        let mut a = app();
        a.view = View::Firehose;
        a.live(state("reviewer", AgentState::BlockedPermission));
        a.live(delivery(
            "m-1",
            "implementer",
            DeliveryState::AttentionRequired,
        ));
        assert_eq!(a.attention_count(), 2);
        assert!(
            a.unreachable(&a.visible()).is_empty(),
            "both lines are here"
        );

        // A filter that admits neither line leaves both counts stranded.
        a.filter.with = Some(endpoint("docs"));
        let stranded = a.unreachable(&a.visible());
        assert_eq!(stranded.len(), 2);
        assert_eq!(stranded[0].name(), "implementer");
        assert_eq!(stranded[1].name(), "reviewer");

        // A filter that admits one leaves the other.
        a.filter.with = Some(endpoint("reviewer"));
        let stranded = a.unreachable(&a.visible());
        assert_eq!(stranded.len(), 1);
        assert_eq!(stranded[0].name(), "implementer");

        // The admin view hides nothing that needs a human, by design.
        a.filter.with = None;
        a.view = View::Admin;
        assert!(a.unreachable(&a.visible()).is_empty());
    }

    /// A line for the right pane but the wrong state is not evidence: the
    /// register moved on, and pointing a reader at a stale line is worse
    /// than saying nothing.
    #[test]
    fn an_older_line_for_the_same_pane_is_not_the_line_behind_the_count() {
        let mut a = app();
        a.view = View::Firehose;
        a.replay(state("reviewer", AgentState::BlockedModal));
        a.seed_status(seed(
            &[("reviewer", "%1", AgentState::BlockedPermission)],
            Vec::new(),
        ));
        let stranded = a.unreachable(&a.visible());
        assert_eq!(stranded.len(), 1, "the stale modal line stood in for it");
        // Which is exactly why the seed wrote a line for it.
        let lines = a.seed_status(seed(
            &[("reviewer", "%1", AgentState::BlockedPermission)],
            Vec::new(),
        ));
        for e in lines {
            a.replay(e);
        }
        assert!(a.unreachable(&a.visible()).is_empty());
    }

    #[test]
    fn routine_gate_holds_stay_out_of_the_calm_view() {
        let mut a = app();
        // Queued behind a turn: routine, firehose only.
        a.live(gate("reviewer", "hold", "working"));
        // Human scrolling in copy-mode: routine too.
        a.live(gate("reviewer", "hold", "pane_in_mode"));
        // A blocked pane is the human's to clear.
        a.live(gate("reviewer", "hold", "blocked:trust_dialog"));
        assert_eq!(a.visible().len(), 1);
        a.view = View::Firehose;
        assert_eq!(a.visible().len(), 3, "the firehose keeps every hold");
    }

    #[test]
    fn keys_drive_view_density_input_and_quit() {
        let mut a = app();
        let workspace = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        a.filter_routes = vec![
            EndpointFilter::new(cyclops_proto::RecipientKey::admin(workspace), "reviewer"),
            EndpointFilter::new(cyclops_proto::RecipientKey::admin(workspace), "x"),
        ];
        assert_eq!(a.handle_key(Key::Char('q')), Some(Command::Quit));
        assert_eq!(a.handle_key(Key::Tab), None);
        assert_eq!(a.view, View::Firehose);
        a.handle_key(Key::Char('c'));
        assert_eq!(a.density, Density::Compact);
        a.handle_key(Key::Char('?'));
        assert!(a.overlay);
        a.handle_key(Key::Esc);
        assert!(!a.overlay);

        // Filter input: w, type, enter.
        a.handle_key(Key::Char('w'));
        for c in "reviewer".chars() {
            a.handle_key(Key::Char(c));
        }
        a.handle_key(Key::Enter);
        assert_eq!(
            a.filter.with.as_ref().map(EndpointFilter::label),
            Some("reviewer")
        );
        // from replaces with (they conflict, like the history flags).
        a.handle_key(Key::Char('f'));
        a.handle_key(Key::Char('x'));
        a.handle_key(Key::Enter);
        assert_eq!(a.filter.from.as_ref().map(EndpointFilter::label), Some("x"));
        assert!(a.filter.with.is_none());
        // Empty input clears the filter.
        a.handle_key(Key::Char('f'));
        a.handle_key(Key::Backspace);
        a.handle_key(Key::Enter);
        assert!(a.filter.from.is_none());

        a.handle_key(Key::Char('t'));
        for c in "renamed-away".chars() {
            a.handle_key(Key::Char(c));
        }
        a.handle_key(Key::Enter);
        assert!(a.filter.to.is_none());
        assert!(a.input.is_some(), "an unknown endpoint must stay editable");
        assert_eq!(
            a.notice.as_deref(),
            Some("unknown mailbox endpoint 'renamed-away'")
        );
    }

    #[test]
    fn selection_unpins_and_end_repins() {
        let mut a = app();
        a.view = View::Firehose;
        for _ in 0..5 {
            a.live(msg("codex", &["reviewer"]));
        }
        assert!(a.pinned);
        a.handle_key(Key::Up);
        assert!(!a.pinned);
        let first = a.selected;
        a.handle_key(Key::Up);
        assert_ne!(a.selected, first);
        a.handle_key(Key::End);
        assert!(a.pinned);
        assert_eq!(a.selected, None);
        // Down walks back to the tail, then one more repins.
        a.handle_key(Key::Up);
        a.handle_key(Key::Down);
        assert!(!a.pinned);
        a.handle_key(Key::Down);
        assert!(a.pinned);
    }

    #[test]
    fn enter_jumps_via_the_harvested_pane_map() {
        let mut a = app();
        a.view = View::Firehose;
        a.seed_status(seed(&[("codex", "%7", AgentState::Idle)], Vec::new()));
        a.live(msg("codex", &["reviewer"]));
        assert_eq!(a.handle_key(Key::Enter), Some(Command::Focus("%7".into())));
        // Unknown label: a notice, no command.
        a.live(msg("ghost", &["reviewer"]));
        assert_eq!(a.handle_key(Key::Enter), None);
        assert!(a.notice.as_deref().unwrap_or("").contains("ghost"));
        // A pane-id entry needs no map.
        a.live(state("reviewer", AgentState::Working));
        assert_eq!(a.handle_key(Key::Enter), Some(Command::Focus("%1".into())));
    }
}
