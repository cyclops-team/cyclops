//! UI state: the entry ring, views, filters, selection, density, and the
//! eye. Pure state transitions here; no IO, no terminal, so every behavior
//! is unit-testable.
//!
//! What needs a human is NOT decided here. The register and the rule live
//! in `cyclops_proto::attention`; this file only feeds it the two things
//! it accepts (the daemon's snapshot, and live events) and asks it for the
//! count. Which is why every entry arrives through a method that names its
//! source: [`App::replay`] for history, [`App::live`] for the push.

use std::collections::{HashMap, HashSet, VecDeque};

use cyclops_proto::{
    Attention, AttentionItem, Clearance, Eye, Half, NotifyLevel, OpenDelivery, PaneSnapshot,
    Resolved,
};

use crate::entry::{Entry, EntryKind, Filter, PingDelivery};
use crate::grid;
use crate::input::Key;
use crate::theme::Theme;

/// Ring capacity. The stream stays fluid past this because rendering is
/// windowed; older entries stay in the ledger, which is the record anyway.
pub const RING_CAP: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Admin,
    Firehose,
}

impl View {
    pub fn words(self) -> &'static str {
        match self {
            View::Admin => "admin stream",
            View::Firehose => "firehose",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Comfortable,
    Compact,
}

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
        }
    }
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
            Which::With => filter.with.as_deref(),
            Which::From => filter.from.as_deref(),
            Which::To => filter.to.as_deref(),
        }
    }
}

/// What a handled key asks the runtime to do beyond redrawing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Quit,
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
    /// Set when the daemon connection dies; the header says so.
    pub conn_lost: bool,
    entries: VecDeque<Entry>,
    next_uid: u64,
    attention: Attention,
    eye: Eye,
    /// Agent label -> pane id, harvested from status and events only
    /// (zero polling). Backs the focus jump.
    panes: HashMap<String, String>,
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
            conn_lost: false,
            entries: VecDeque::new(),
            next_uid: 1,
            attention: Attention::default(),
            eye: Eye::Closed,
            panes: HashMap::new(),
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

    /// The eye as currently drawn (it may still be mid-tick).
    pub fn eye(&self) -> Eye {
        self.eye
    }

    /// Where the eye is headed given current attention.
    pub fn eye_target(&self) -> Eye {
        self.attention.eye()
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
    /// nowhere else.
    ///
    /// History cannot answer "what needs a human right now". Letting it
    /// try is exactly what made `--backfill` decide the count: the same
    /// rig read at 200 lines and at 400 gave a closed eye and an open one.
    /// The daemon's snapshot and the live push own the register.
    pub fn replay(&mut self, e: Entry) {
        self.ingest(e);
    }

    /// One live event from the daemon: it goes on the screen AND moves the
    /// register, because the event is the pane's or the delivery's own
    /// next transition.
    ///
    /// Returns the clearance line when the transition ended something that
    /// needed a human (`cyclops_proto::attention`, rule 3). It is already
    /// on the stream; the caller gets it because `--plain` prints line by
    /// line and has no frame to reconcile (plain.rs), so a line it never
    /// sees is a line it never prints.
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
            // A pane leaving the table is its last transition. The jump
            // map goes with it: "no pane known for reviewer" is honest,
            // and a jump to a pane id tmux has retired is not.
            EntryKind::PaneGone { pane_id } => {
                self.panes.retain(|_, p| p != pane_id);
                self.attention.forget_agent(pane_id)
            }
            _ => None,
        };
        let (ts, id) = (e.ts, e.id.clone());
        self.ingest(e);
        // Nothing ended: the transition is on the stream and that is all
        // there is to say about it.
        let resolved = resolved?;
        self.ingest(Entry::cleared(ts, id, resolved));
        // Copied back off the ring so the caller's entry carries the uid
        // the ring gave it, not a placeholder.
        self.entries.back().cloned()
    }

    /// Assign a uid, harvest the label -> pane map, ring the buffer.
    /// Selection anchors survive eviction because they hold uids, not
    /// indices.
    ///
    /// The pane map is navigation, not counting: a replayed line may name
    /// a pane the jump can use. The seed lands after the replayed tail and
    /// live events after the seed, so the newest naming always wins.
    fn ingest(&mut self, mut e: Entry) {
        e.uid = self.next_uid;
        self.next_uid += 1;
        if let EntryKind::State {
            target,
            pane_id: Some(p),
            ..
        } = &e.kind
        {
            self.panes.insert(target.clone(), p.clone());
        }
        if self.entries.len() == RING_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(e);
    }

    /// Startup reconciliation from the daemon's one status answer: the
    /// label -> pane map, where every pane stands now, and every delivery
    /// still waiting on a human.
    ///
    /// The answer REPLACES the register. It is a snapshot of now, folded
    /// from the whole record on the delivery side and read off the live
    /// pane table on the agent side, so anything it does not list is
    /// resolved or gone. Merging into it left a blocked pane that later
    /// disappeared counted for the life of the process, with no event able
    /// to clear it.
    ///
    /// Returns the entries the caller should ingest, in two directions.
    /// One line for each seeded item the loaded stream does not already
    /// account for, so no number the header shows is left without a line
    /// behind it; and one clearance for each alarm the loaded stream DOES
    /// show that the answer no longer counts, so no line saying a human is
    /// needed is left without the news that it ended. The caller pushes
    /// them, because in `--plain` they must also print.
    ///
    /// Event-driven only: called once at startup and never on a timer.
    /// After that the register moves on live events alone, and a pane
    /// leaving the table is one of them ([`App::live`]).
    pub fn seed_status(&mut self, seed: StatusSeed) -> Vec<Entry> {
        // 1. Replace both halves of the register with the answer.
        self.attention.snapshot_agents(seed.panes.iter().cloned());
        self.attention.snapshot_deliveries(&seed.open);
        // 2. Refresh the jump map, which outlives the roster: a label the
        //    answer still names points at the pane the answer named.
        for p in &seed.panes {
            self.panes.insert(p.name.clone(), p.pane_id.clone());
        }
        // 3. Write a line for every counted item the replayed tail does
        //    not already carry. The register says WHICH items those are,
        //    so the header and the stream cannot disagree about the list;
        //    the answer supplies when each one happened.
        //
        //    Both lookups are indexed once, not searched per item: the
        //    backlog is the item count and a quota weekend leaves hundreds
        //    of parked deliveries, so a scan per item costs items squared.
        let items = self.attention.items();
        let newest = self.newest_line_per_item(&items);
        let open: HashMap<(&str, &str), &OpenDelivery> = seed
            .open
            .iter()
            .map(|d| ((d.to.as_str(), d.id.as_str()), d))
            .collect();
        let mut out = Vec::new();
        for item in &items {
            if newest
                .get(&item.identity())
                .is_some_and(|e| says_the_same(e, item))
            {
                continue; // the stream already says this
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
                    let record = open.get(&(to.as_str(), id.as_str())).copied();
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
        // 4. And the mirror of step 3. The replayed tail can hold an alarm
        //    whose item the answer does not count: a park requeued while
        //    the UI was down, a pane that unblocked or went away. Its line
        //    is on the screen saying a human is needed, and the transition
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
        out
    }

    /// Every alarm the loaded stream still shows with nothing under it,
    /// paired with the register's account of how it ended.
    ///
    /// One walk of the ring, oldest first, holding the newest alarm per
    /// item: a later alarm about the same item supersedes an earlier one
    /// (one pane has one current state), and a clearance already on the
    /// stream retires it. Sorted by name so a startup over a long tail
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

    /// Counted items with no line in the current view.
    ///
    /// The header may never show a number the reader cannot reach, so
    /// whatever this returns has to be said on the frame. Two things put
    /// an item here: a filter that hides its line, and eviction from the
    /// 10k ring. The startup reconciliation writes a line for everything
    /// else, so nothing else can.
    ///
    /// One walk of the view, not one per item. The band rides EVERY frame
    /// and `visible` is the whole filtered ring, so a scan per item makes
    /// the frame cost items x ring.
    ///
    /// Measured against the naive scan over a full 10,000-entry firehose:
    /// 7.0ms at 50 items, 13.3ms at 100, 15.8ms at 120, 19.6ms at 150,
    /// 51ms at 400. The 16ms frame budget goes at roughly 125 items, not
    /// at the 400 the perf list happens to start failing on, and a quota
    /// weekend leaves hundreds. The admin view stays cheap either way
    /// (2.5ms at 400) because it filters the ring down first, and it is
    /// the default view, which is why the firehose is what this is sized
    /// for. Numbers from crates/cyclops-ui/tests/perf.rs on the dev
    /// machine; the test asserts the budget rather than these times.
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

    /// Entries the current view and filter admit, oldest first.
    pub fn visible(&self) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|e| self.admits_in_view(e))
            .filter(|e| self.filter.matches(e))
            .collect()
    }

    /// Does the CURRENT view admit this line? The firehose admits
    /// everything; the admin stream asks [`App::admits`].
    pub fn admits_in_view(&self, e: &Entry) -> bool {
        self.view == View::Firehose || self.admits(e)
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
    fn admits(&self, e: &Entry) -> bool {
        if !e.admin_visible() {
            return false;
        }
        let mut items = ping_items(e).peekable();
        if items.peek().is_none() {
            return true; // names nothing the register could answer for
        }
        items.any(|item| self.attention.holds(item))
    }

    /// Handle one key. Returns a command for the runtime when the key
    /// asks for more than state change.
    pub fn handle_key(&mut self, key: Key) -> Option<Command> {
        // An open input line captures everything except control keys.
        if self.input.is_some() {
            return self.handle_input_key(key);
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
                    View::Firehose => View::Admin,
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
        let input = self.input.as_mut().expect("input line open");
        match key {
            Key::Enter => {
                let value = input.buf.trim().to_string();
                let value = if value.is_empty() { None } else { Some(value) };
                match input.which {
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
                input.buf.pop();
            }
            Key::Char(c) if !c.is_control() => input.buf.push(c),
            Key::CtrlC => return Some(Command::Quit),
            _ => {}
        }
        None
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
            Some(pane) => Some(pane.clone()),
            None => {
                self.notice = Some(format!("no pane known for {name}"));
                None
            }
        }
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
/// (`App::admits` through `App::visible`), and a firehose over a full ring
/// is where the frame budget goes.
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
    use crate::entry::EntryKind;
    use cyclops_proto::{AgentState, DeliveryState};

    fn msg(from: &str, to: &[&str]) -> Entry {
        Entry {
            uid: 0,
            ts: 1000,
            seq: None,
            id: Some("m-1".into()),
            kind: EntryKind::Msg {
                from: from.into(),
                to: to.iter().map(|t| t.to_string()).collect(),
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
        a.filter.with = Some("docs".into());
        let stranded = a.unreachable(&a.visible());
        assert_eq!(stranded.len(), 2);
        assert_eq!(stranded[0].name(), "implementer");
        assert_eq!(stranded[1].name(), "reviewer");

        // A filter that admits one leaves the other.
        a.filter.with = Some("reviewer".into());
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
        assert_eq!(a.filter.with.as_deref(), Some("reviewer"));
        // from replaces with (they conflict, like the history flags).
        a.handle_key(Key::Char('f'));
        a.handle_key(Key::Char('x'));
        a.handle_key(Key::Enter);
        assert_eq!(a.filter.from.as_deref(), Some("x"));
        assert!(a.filter.with.is_none());
        // Empty input clears the filter.
        a.handle_key(Key::Char('f'));
        a.handle_key(Key::Backspace);
        a.handle_key(Key::Enter);
        assert!(a.filter.from.is_none());
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
