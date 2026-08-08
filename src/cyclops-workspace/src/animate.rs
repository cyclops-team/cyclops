//! The workspace's animation clock: bounded foreground-color fades and the
//! one-shot wakeups that drive them.
//!
//! Four things fade, all of them chrome the workspace owns and none of them
//! a cell tmux owns: a pane border taking or losing focus, an agent's status
//! ink, the attention eye arriving, and the notice dissolving on the last
//! stretch of its TTL. Nothing here moves a rectangle.
//!
//! THE ONE RULE: in a cell grid, animate color and never position. Position
//! quantizes to one cell and foreground color quantizes to one part in 256,
//! so a fade reads as motion and a slide reads as a shear. The numbers, so
//! the next person reaching for a sliding sidebar does not have to rederive
//! them: 22 columns over 140ms at this cadence is 2.5 cells per frame, and
//! apparent motion breaks down once per-frame displacement exceeds the
//! feature being displaced, which here is a one-cell glyph. A fill
//! crossfade is out for a second reason: it moves both sides of a contrast
//! pair, and panel ink on panel fading to panel ink on accent measures
//! 1.69:1 at t=0.5 in the shipped dark theme, a midpoint nobody measured.
//! Only the figure moves; grounds switch instantly.
//!
//! Two consequences worth naming. No animation delays the arrival of
//! information: glyphs, words and text are at their final value on the
//! first frame and only the ink travels, so rule 11's non-color encodings
//! are untouched. And hit geometry never moves, so no input path needs to
//! know this module exists.
//!
//! RULE 9 (docs/development/INVARIANTS.md). This is the only timer in the
//! product that re-arms itself, and the rule was amended to name it rather
//! than reinterpreted to admit it. What keeps it honest:
//!
//! 1. It arms only from [`Motion::observe`], which runs from `app::draw`,
//!    which runs from a deadline a real event armed. [`Motion::arm`] is
//!    private. No animation can start an animation, so the set cannot
//!    sustain itself. The guard on that is a borrow: `app::observed` takes
//!    `&App`, never `&mut`, so a draw cannot write something the next
//!    diff would read back as a change.
//! 2. Every animation's end instant is fixed when it is armed. Nothing
//!    extends one. Re-triggering the same key REPLACES it, starting from
//!    the value currently on screen, so the longest a fade can run is its
//!    own duration.
//! 3. An empty set has no deadline. [`Motion::deadline`] returns `None`,
//!    the loop's `min` over the deadline set skips it, and the workspace
//!    parks on `rx.recv()` with no timer at all.
//! 4. Nothing here observes the world. A frame this clock asks for is
//!    output, so it cannot paper over an event path that stopped
//!    delivering. That is the second harm rule 9 names and the one an
//!    interval usually hides.
//! 5. With color off there is nothing to interpolate, so the clock is
//!    disabled outright and arms nothing (`Paint::colors_enabled`).
//!
//! The clock stores time, never color. Endpoints are resolved through the
//! live `Paint` on every frame, which is why a theme change mid-fade lands
//! on the new colors instead of interpolating toward one the theme dropped.
//!
//! STANDING CONDITION on the default being on: it is justified by a
//! property of the list above, not of the mechanism. Nothing translates,
//! scales or scrolls, nothing repeats, nothing inverts, and the fastest
//! fade completes in 120ms, nowhere near the 3Hz photosensitivity
//! threshold. If a later change adds a moving rectangle, the default flips
//! to off in the same commit.

use tokio::time::{Duration, Instant};

/// One frame every 16ms while something is running. Deliberately not the
/// loop's 8ms `RENDER_DEBOUNCE`: that bound exists to collapse event
/// bursts, and paying it per frame would double the terminal writes a fade
/// costs without adding a step a reader can see.
const MOTION_FRAME: Duration = Duration::from_millis(16);

/// Ceiling on animations in flight. Eight panes carry a focus pair and a
/// status fade each, plus the notice, so this is well past what the widest
/// rig can produce; it exists to bound the linear scan every lookup does.
const MAX_ANIMS: usize = 32;

/// A pane border taking or losing the accent. Eight frames, ease-out: 35
/// percent of the distance lands on the first frame, so it reads as
/// response plus a settle rather than as a delay.
pub const FOCUS: Duration = Duration::from_millis(120);

/// An agent's status ink settling onto its new state color.
pub const STATUS: Duration = Duration::from_millis(120);

/// The attention eye arriving. Slower on purpose: the slowness is the
/// signal. Arrival only, never a repeat, because attention persists until
/// a human clears it and a pulse would wake an idle workspace for hours,
/// which is the idle burn rule 9 names.
pub const ATTENTION: Duration = Duration::from_millis(320);

/// The last stretch of a notice's TTL, over which the phrase dissolves into
/// its own ground. Appearance stays instant: feedback that fades in is
/// feedback that arrived late.
pub const NOTICE_LEAVE: Duration = Duration::from_millis(260);

/// Three consecutive draws over this while something was animating, and
/// motion latches off for the session. MEASURED: composing a full 8-pane
/// 256x51 frame is 570us (`render/canvas.rs`), so a 4ms draw is the
/// terminal's write and flush, not ours, and at the 16ms cadence a quarter
/// of every frame is going into decoration.
const SLOW_FRAME: Duration = Duration::from_millis(4);

/// Three in a row, not one, so a scheduler hiccup or the full repaint after
/// a resize does not latch it.
const SLOW_FRAMES_TO_LATCH: u8 = 3;

/// Where a status cell's ink came from before it changed: an agent state,
/// or the attention eye. The SEMANTIC source, never a resolved color, so a
/// theme swap mid-fade lands on the new endpoints instead of interpolating
/// toward a color the theme no longer has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusInk {
    Eye,
    State(cyclops_proto::AgentState),
}

/// One animated cell, named by what it is rather than by what color it
/// carries.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    /// How much accent this pane's border carries. 1.0 focused.
    Focus(String),
    /// How far this pane's status cell has settled onto its new ink.
    Status { pane: String, from: StatusInk },
    /// How present the transient notice is. 1.0 fully, 0.0 gone.
    Notice,
}

impl Key {
    /// Two keys naming the same cell. `Status` carries the ink it is
    /// leaving so the painter can resolve it, but a pane has one status
    /// cell, so a second change to it replaces the first rather than
    /// fading twice over the same glyph.
    fn same_slot(&self, other: &Key) -> bool {
        match (self, other) {
            (Key::Focus(a), Key::Focus(b)) => a == b,
            (Key::Status { pane: a, .. }, Key::Status { pane: b, .. }) => a == b,
            (Key::Notice, Key::Notice) => true,
            _ => false,
        }
    }
}

/// Arrivals ease out, departures ease in. Two curves, each one line, each
/// with a stated job; no bezier machinery.
enum Curve {
    /// Cubic ease-out. Puts the big step first, so a 120ms fade reads as
    /// roughly 40ms of response and a settle, and puts the steps below the
    /// just-noticeable difference in the tail where they cost nothing.
    Out,
    /// Quadratic ease-in. A correction, not a taste: `render::lerp`
    /// interpolates in sRGB code space, which is front-loaded against a
    /// dark ground, so a linear departure has lost most of its perceived
    /// brightness at the halfway mark and then lingers as a ghost.
    In,
}

struct Anim {
    key: Key,
    from: f32,
    to: f32,
    start: Instant,
    end: Instant,
    curve: Curve,
}

impl Anim {
    /// The whole interpolator. Scalars only: the painter resolves both
    /// color endpoints itself and multiplies by this.
    fn value(&self, now: Instant) -> f32 {
        if now <= self.start {
            return self.from;
        }
        let span = self.end.saturating_duration_since(self.start).as_secs_f32();
        if span <= 0.0 {
            return self.to;
        }
        let t = (now.saturating_duration_since(self.start).as_secs_f32() / span).clamp(0.0, 1.0);
        let eased = match self.curve {
            Curve::Out => {
                let u = 1.0 - t;
                1.0 - u * u * u
            }
            Curve::In => t * t,
        };
        self.from + (self.to - self.from) * eased
    }
}

/// What one frame showed, so the next frame can arm what changed.
pub struct Seen {
    focus: Option<String>,
    status: Vec<(String, StatusInk)>,
    notice_until: Option<Instant>,
}

impl Seen {
    pub fn new(
        focus: Option<String>,
        status: Vec<(String, StatusInk)>,
        notice_until: Option<Instant>,
    ) -> Self {
        Seen {
            focus,
            status,
            notice_until,
        }
    }

    fn ink(&self, pane: &str) -> Option<StatusInk> {
        self.status
            .iter()
            .find(|(id, _)| id == pane)
            .map(|(_, ink)| *ink)
    }
}

/// The clock. One per workspace, owned by the event loop next to the render
/// debounce and the reconnect deadline.
pub struct Motion {
    enabled: bool,
    anims: Vec<Anim>,
    /// The one instant the loop must wake for, recomputed by [`Self::rearm`].
    wake: Option<Instant>,
    /// What the last painted frame showed. `None` before the first frame.
    seen: Option<Seen>,
    slow_run: u8,
}

impl Motion {
    pub fn new(enabled: bool) -> Self {
        Motion {
            enabled,
            anims: Vec::new(),
            wake: None,
            seen: None,
            slow_run: 0,
        }
    }

    /// Diff this frame against the last one and arm what changed. The only
    /// public way to start an animation.
    ///
    /// Observation rather than an imperative `arm` at each site, because
    /// focus alone changes through at least six routes and status through
    /// two more. A list of call sites is a list a future contributor adds a
    /// seventh entry to and forgets; one diff catches every route, present
    /// and future.
    pub fn observe(&mut self, seen: Seen, now: Instant) {
        // 1. Nothing to diff against. There is no transition into
        //    existence, so boot does not fade its whole chrome in.
        let Some(last) = self.seen.take() else {
            self.seen = Some(seen);
            return;
        };
        // 2. Color off, no truecolor, or opted out: record the frame and
        //    arm nothing, so the clock never owns a deadline.
        if !self.enabled {
            self.seen = Some(seen);
            return;
        }
        // 3. Focus moved. Two animations, one per pane, so the leaving
        //    border settles back to dim while the arriving one takes the
        //    accent. `from` is the value on screen, so a retarget mid-fade
        //    is continuous rather than a snap.
        if last.focus != seen.focus {
            if let Some(left) = &last.focus {
                self.arm(Key::Focus(left.clone()), 1.0, 0.0, FOCUS, Curve::Out, now);
            }
            if let Some(arrived) = &seen.focus {
                self.arm(
                    Key::Focus(arrived.clone()),
                    0.0,
                    1.0,
                    FOCUS,
                    Curve::Out,
                    now,
                );
            }
        }
        // 4. A pane's status ink changed. Keyed by pane id, so the pane
        //    title and the sidebar row showing the same agent settle
        //    together, and keyed by the ink being left so the painter can
        //    resolve that endpoint through the live theme.
        for (pane, ink) in &seen.status {
            let Some(was) = last.ink(pane) else {
                continue;
            };
            if was == *ink {
                continue;
            }
            let span = if *ink == StatusInk::Eye {
                ATTENTION
            } else {
                STATUS
            };
            let key = Key::Status {
                pane: pane.clone(),
                from: was,
            };
            self.arm(key, 0.0, 1.0, span, Curve::Out, now);
        }
        // 5. A notice appeared with more life left than its leave takes.
        //    Armed now, STARTS later: the only animation in the set whose
        //    start instant is in the future, and the only reason `rearm`
        //    ever wakes for something other than the next frame. A notice
        //    already inside its last stretch gets no fade rather than a
        //    clipped one.
        if last.notice_until != seen.notice_until {
            if let Some(until) = seen.notice_until {
                if until > now + NOTICE_LEAVE {
                    self.arm_pending(Key::Notice, until - NOTICE_LEAVE, until);
                }
            }
        }
        self.seen = Some(seen);
        self.rearm(now);
    }

    /// Replace whatever is animating `key` with a fade from the value on
    /// screen to `to`, finishing `span` from now. `rest` is what the key
    /// shows when nothing is armed for it, which is where a fade that is
    /// not a retarget starts.
    fn arm(&mut self, key: Key, rest: f32, to: f32, span: Duration, curve: Curve, now: Instant) {
        let from = self.value_for(&key, now).unwrap_or(rest);
        self.anims.retain(|anim| !anim.key.same_slot(&key));
        self.anims.push(Anim {
            key,
            from,
            to,
            start: now,
            end: now + span,
            curve,
        });
        self.trim();
    }

    /// Arm a departure that has not started yet, from full presence to
    /// gone. The notice leave is the only one: its start is known the
    /// moment the notice appears, and a newer notice replaces it outright
    /// the way a newer notice replaces the message itself.
    fn arm_pending(&mut self, key: Key, start: Instant, end: Instant) {
        self.anims.retain(|anim| !anim.key.same_slot(&key));
        self.anims.push(Anim {
            key,
            from: 1.0,
            to: 0.0,
            start,
            end,
            curve: Curve::In,
        });
        self.trim();
    }

    /// Hold the set at [`MAX_ANIMS`]. The vector is in arm order, so index
    /// 0 is the oldest and the closest to finishing.
    fn trim(&mut self) {
        while self.anims.len() > MAX_ANIMS {
            self.anims.remove(0);
        }
    }

    /// What `key` is showing right now, or `None` when nothing is animating
    /// it. Exact key match, not slot match: a status fade leaving a
    /// different ink is a different animation and its progress means
    /// nothing to this one.
    fn value_for(&self, key: &Key, now: Instant) -> Option<f32> {
        self.anims
            .iter()
            .find(|anim| anim.key == *key)
            .map(|anim| anim.value(now))
    }

    /// The next instant this clock owes the loop. Two sources: the next
    /// frame while something is running, and the start of an animation that
    /// has not begun (the notice leave is the only one). A started
    /// animation's `start` is in the PAST, so it must never enter this
    /// minimum: returning a past instant would make `next_wake` return
    /// `Wake::Deadline` immediately, forever, which is the spin rule 9
    /// exists to prevent.
    fn rearm(&mut self, now: Instant) {
        let frame = self
            .anims
            .iter()
            .any(|anim| anim.start <= now)
            .then(|| now + MOTION_FRAME);
        let next_start = self
            .anims
            .iter()
            .map(|anim| anim.start)
            .filter(|start| *start > now)
            .min();
        self.wake = [frame, next_start].into_iter().flatten().min();
    }

    /// The deadline the event loop must wake for, or `None` when nothing is
    /// running. One field, written only by [`Self::rearm`].
    pub fn deadline(&self) -> Option<Instant> {
        self.wake
    }

    /// Retire finished animations and arm the next frame if any remain.
    /// Returns whether this wake owes a frame: true while anything is
    /// running, and once more on the wake that retires the last one, so the
    /// resting state is painted exactly once and never again.
    pub fn tick(&mut self, now: Instant) -> bool {
        let owed = self.anims.iter().any(|anim| anim.start <= now);
        self.anims.retain(|anim| anim.end > now);
        self.rearm(now);
        owed
    }

    /// Drop everything in flight and stop owing the loop a wakeup. The
    /// slow-terminal latch is the only caller.
    pub fn settle(&mut self) {
        self.anims.clear();
        self.wake = None;
    }

    /// Count one draw against the slow-terminal latch. Returns true on the
    /// single wake where motion latches off, so the caller writes one line
    /// to `workspace.log` and this module stays free of IO.
    ///
    /// Follow the operator's preference, cancelling anything mid-fade when
    /// it goes false.
    ///
    /// Separate from the capability gates that [`Motion::new`] is
    /// constructed with, and one-directional against them: those answer
    /// "can this terminal fade", this answers "should it", and a
    /// preference must never re-enable motion on a terminal that cannot
    /// paint it. So a `false` capability wins permanently, which is also
    /// why the slow-terminal latch in [`Self::note_frame`] is safe from
    /// being undone by a menu click.
    pub fn set_preference(&mut self, wanted: bool, capable: bool) {
        let next = wanted && capable;
        if next == self.enabled {
            return;
        }
        self.enabled = next;
        if !next {
            // Mid-fade when the switch flips: land on the endpoints now
            // rather than freezing the chrome at some fraction of the way
            // there, and drop the wakeup that was driving it.
            self.settle();
        }
    }

    /// One way. Nothing re-enables it, so it cannot oscillate against a
    /// terminal that is intermittently slow; a user who fixes their link
    /// restarts the workspace.
    pub fn note_frame(&mut self, cost: Duration) -> bool {
        // Only draws that were paying for motion count. A slow frame with
        // nothing animating is somebody else's cost.
        if !self.enabled || self.anims.is_empty() {
            return false;
        }
        if cost < SLOW_FRAME {
            self.slow_run = 0;
            return false;
        }
        self.slow_run += 1;
        if self.slow_run < SLOW_FRAMES_TO_LATCH {
            return false;
        }
        self.enabled = false;
        self.settle();
        true
    }
}

/// What one frame reads. `Copy`, holds no owned state, and lives exactly as
/// long as the `draw` that built it.
#[derive(Clone, Copy)]
pub struct MotionFrame<'a> {
    anims: &'a [Anim],
    now: Instant,
}

impl<'a> MotionFrame<'a> {
    pub fn new(motion: &'a Motion, now: Instant) -> Self {
        MotionFrame {
            anims: &motion.anims,
            now,
        }
    }

    /// The resting frame: every lookup answers with the value at rest. What
    /// render tests paint through, and what a disabled clock produces
    /// anyway, since it arms nothing.
    pub fn none() -> Self {
        MotionFrame {
            anims: &[],
            now: Instant::now(),
        }
    }

    /// How much accent this pane's border carries. Rest is the pane's own
    /// focus state, so a frame with nothing armed paints exactly what it
    /// painted before this module existed.
    pub fn focus(&self, pane_id: &str, focused: bool) -> f32 {
        self.first(|key| matches!(key, Key::Focus(pane) if pane == pane_id))
            .unwrap_or(if focused { 1.0 } else { 0.0 })
    }

    /// The ink this pane's status cell is leaving and how far it has left,
    /// or `None` when the cell is at rest.
    pub fn status(&self, pane_id: &str) -> Option<(StatusInk, f32)> {
        self.anims.iter().find_map(|anim| match &anim.key {
            Key::Status { pane, from } if pane == pane_id => Some((*from, anim.value(self.now))),
            _ => None,
        })
    }

    /// How present the notice is. Rest is 1.0: a notice that is showing at
    /// all is fully present until its leave starts.
    pub fn notice(&self) -> f32 {
        self.first(|key| matches!(key, Key::Notice)).unwrap_or(1.0)
    }

    fn first(&self, matches: impl Fn(&Key) -> bool) -> Option<f32> {
        self.anims
            .iter()
            .find(|anim| matches(&anim.key))
            .map(|anim| anim.value(self.now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyclops_proto::AgentState;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// One frame's worth of observation, with the pane ids the tests use.
    fn seen(focus: &str, ink: StatusInk, notice_until: Option<Instant>) -> Seen {
        Seen::new(
            Some(focus.to_string()),
            vec![("%0".to_string(), ink)],
            notice_until,
        )
    }

    fn idle(focus: &str) -> Seen {
        seen(focus, StatusInk::State(AgentState::Idle), None)
    }

    /// The rule 9 test. An unchanged workspace arms nothing, so
    /// `deadline()` stays `None`, the loop's `min` over the deadline set
    /// skips it, and the workspace parks on `rx.recv()` with no timer at
    /// all. Zero wakeups, byte for byte the behavior before this module.
    #[test]
    fn an_idle_set_arms_no_wakeup() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        assert_eq!(motion.deadline(), None, "a fresh clock owes nothing");

        motion.observe(idle("%0"), now);
        assert_eq!(motion.deadline(), None, "the first frame only seeds");

        // Every later frame with the same focus, the same ink and no
        // notice: nothing moved, so nothing is armed.
        for step in 1..5 {
            motion.observe(idle("%0"), now + ms(step * 40));
            assert_eq!(
                motion.deadline(),
                None,
                "an unchanged frame must not arm a wakeup"
            );
        }
        assert!(
            !motion.tick(now + ms(200)),
            "and a wake that belonged to something else owes no frame"
        );
        assert_eq!(motion.deadline(), None);
    }

    #[test]
    fn the_first_observation_seeds_and_arms_nothing() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        // Focus, ink and notice are all "new" here, and none of them is a
        // transition: there is no previous frame to have come from.
        motion.observe(seen("%1", StatusInk::Eye, Some(now + ms(3500))), now);
        assert_eq!(motion.deadline(), None);
        assert!(motion.anims.is_empty());
    }

    /// The menu switch cancels a fade in flight rather than freezing the
    /// chrome part-way to its endpoint, and it drops the wakeup with it.
    #[test]
    fn turning_motion_off_mid_fade_settles_and_disarms() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        motion.observe(idle("a"), now);
        motion.observe(idle("b"), now);
        assert!(
            motion.deadline().is_some(),
            "a focus change should have armed a fade to cancel"
        );

        motion.set_preference(false, true);
        assert_eq!(
            motion.deadline(),
            None,
            "the switch must take the wakeup with it, not leave a timer running"
        );
    }

    /// The preference answers "should we" and the capability gates answer
    /// "can we". A preference must never win over a terminal that cannot
    /// paint the fade, or turning motion on under 256 colors would paint
    /// the banding the capability gate exists to refuse.
    #[test]
    fn a_preference_cannot_re_enable_motion_an_incapable_terminal_refused() {
        let now = Instant::now();
        let mut motion = Motion::new(false);
        motion.set_preference(true, false);

        motion.observe(idle("a"), now);
        motion.observe(idle("b"), now);
        assert_eq!(
            motion.deadline(),
            None,
            "wanting motion on a terminal that cannot show it must arm nothing"
        );
    }

    #[test]
    fn a_disabled_clock_arms_nothing_and_has_no_deadline() {
        let now = Instant::now();
        let mut motion = Motion::new(false);
        motion.observe(idle("%0"), now);
        motion.observe(
            seen("%1", StatusInk::Eye, Some(now + ms(3500))),
            now + ms(10),
        );
        assert!(motion.anims.is_empty(), "color off, nothing to interpolate");
        assert_eq!(motion.deadline(), None);
        assert!(!motion.tick(now + ms(10)));
    }

    /// The disarm half of rule 9: the last animation's final wake paints
    /// the resting state once, and every wake after it owes nothing.
    #[test]
    fn the_last_animation_paints_one_resting_frame_then_disarms() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        motion.observe(idle("%0"), now);
        motion.observe(idle("%1"), now);

        assert_eq!(
            motion.deadline(),
            Some(now + MOTION_FRAME),
            "a focus change owes the next frame"
        );
        // Mid-fade the set is still live and still asking for frames.
        let mid = now + ms(64);
        assert!(motion.tick(mid), "a running fade owes this frame");
        assert_eq!(motion.deadline(), Some(mid + MOTION_FRAME));

        // The wake past the end retires both anims and still owes the one
        // frame that paints the resting state.
        let done = now + FOCUS + MOTION_FRAME;
        assert!(motion.tick(done), "the resting state is painted once");
        assert_eq!(motion.deadline(), None, "and then the clock is quiet");
        assert!(!motion.tick(done + MOTION_FRAME), "never again");
        assert_eq!(motion.deadline(), None);
    }

    /// The mechanical bound the rule 9 amendment cites: arm every key at
    /// once, run past the longest duration, and the set is empty and stays
    /// empty however many times it is ticked.
    #[test]
    fn the_clock_empties_within_its_longest_animation() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        motion.observe(idle("%0"), now);
        // Focus moves, the status ink becomes the eye (the 320ms one), and
        // a notice appears whose leave is armed for later.
        motion.observe(seen("%1", StatusInk::Eye, Some(now + ms(3500))), now);
        assert!(motion.deadline().is_some());

        let longest = now + ms(3500) + MOTION_FRAME;
        assert!(motion.tick(longest));
        assert_eq!(motion.deadline(), None);
        for step in 1..4 {
            assert!(
                !motion.tick(longest + MOTION_FRAME * step),
                "an empty set owes no frame"
            );
            assert_eq!(motion.deadline(), None);
        }
    }

    /// A focus change during a focus fade starts from the value on screen,
    /// so the border never snaps back to dim before settling again.
    #[test]
    fn a_retarget_starts_from_the_value_on_screen() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        motion.observe(idle("%0"), now);
        motion.observe(idle("%1"), now);

        // Halfway into %1 taking the accent, focus goes back to %0.
        let mid = now + ms(60);
        let on_screen = MotionFrame::new(&motion, mid).focus("%1", true);
        assert!(
            on_screen > 0.0 && on_screen < 1.0,
            "mid-fade, not an endpoint: {on_screen}"
        );
        motion.observe(idle("%0"), mid);
        let after = MotionFrame::new(&motion, mid).focus("%1", false);
        assert!(
            (after - on_screen).abs() < f32::EPSILON,
            "the retarget starts where the last frame left off: {after} vs {on_screen}"
        );
        // And it lands on the resting value of the pane that lost focus.
        let settled = MotionFrame::new(&motion, mid + FOCUS).focus("%1", false);
        assert!(settled.abs() < f32::EPSILON, "{settled}");
    }

    /// The pending leave is the one animation armed before it starts. It
    /// must wake the loop exactly once at that start, and not before.
    #[test]
    fn a_pending_notice_leave_wakes_once_at_its_start() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        motion.observe(idle("%0"), now);

        let until = now + ms(3500);
        motion.observe(
            seen("%0", StatusInk::State(AgentState::Idle), Some(until)),
            now,
        );
        let start = until - NOTICE_LEAVE;
        assert_eq!(
            motion.deadline(),
            Some(start),
            "nothing is running, so the only wake owed is the leave's start"
        );
        // Full presence until then, and no frames asked for in between.
        assert_eq!(MotionFrame::new(&motion, now + ms(1000)).notice(), 1.0);
        assert!(!motion.tick(now + ms(1000)), "not this wake");
        assert_eq!(motion.deadline(), Some(start));

        // From the start instant on it is a running fade like any other.
        assert!(motion.tick(start));
        assert_eq!(motion.deadline(), Some(start + MOTION_FRAME));
        let fading = MotionFrame::new(&motion, start + ms(130)).notice();
        assert!(fading > 0.0 && fading < 1.0, "{fading}");
        assert!(motion.tick(until));
        assert_eq!(motion.deadline(), None);
    }

    /// A notice already inside its last stretch gets no fade. Arming one
    /// would put a start instant in the past, and a past deadline makes
    /// `next_wake` fire immediately, forever.
    #[test]
    fn a_started_animation_never_puts_a_past_instant_in_the_deadline() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        motion.observe(idle("%0"), now);
        motion.observe(
            seen(
                "%0",
                StatusInk::State(AgentState::Idle),
                Some(now + NOTICE_LEAVE / 2),
            ),
            now,
        );
        assert!(motion.anims.is_empty(), "too late to dissolve");
        assert_eq!(motion.deadline(), None);

        // And for animations that ARE running, every deadline the clock
        // hands the loop is strictly in the future.
        motion.observe(idle("%1"), now);
        let mut at = now;
        for _ in 0..16 {
            let Some(wake) = motion.deadline() else { break };
            assert!(wake > at, "a deadline in the past spins the loop");
            at = wake;
            motion.tick(at);
        }
        assert_eq!(motion.deadline(), None, "and it terminates");
    }

    /// The eye is the slow one, and the difference is chosen at the arming
    /// site rather than by the painter.
    #[test]
    fn the_eye_arrives_slower_than_a_state_change() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        motion.observe(idle("%0"), now);

        motion.observe(seen("%0", StatusInk::State(AgentState::Working), None), now);
        let (from, _) = MotionFrame::new(&motion, now)
            .status("%0")
            .expect("the ink it is leaving");
        assert_eq!(from, StatusInk::State(AgentState::Idle));
        // A 120ms fade is done here; a 320ms one is not.
        assert!(motion.tick(now + STATUS + MOTION_FRAME));
        assert_eq!(motion.deadline(), None);

        motion.observe(seen("%0", StatusInk::Eye, None), now + ms(200));
        assert!(MotionFrame::new(&motion, now + ms(200) + STATUS)
            .status("%0")
            .is_some_and(|(_, t)| t < 1.0));
    }

    /// The latch measures the whole draw, needs three in a row, and fires
    /// once.
    #[test]
    fn three_slow_draws_latch_motion_off() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        motion.observe(idle("%0"), now);
        motion.observe(idle("%1"), now);

        assert!(!motion.note_frame(ms(6)));
        assert!(!motion.note_frame(ms(6)));
        // One fast frame and the run restarts: a hiccup must not latch.
        assert!(!motion.note_frame(ms(1)));
        assert!(!motion.note_frame(ms(9)));
        assert!(!motion.note_frame(ms(9)));
        assert!(motion.note_frame(ms(9)), "the third in a row latches");
        assert_eq!(motion.deadline(), None, "and settles what was running");

        motion.observe(idle("%0"), now + ms(100));
        assert!(motion.anims.is_empty(), "one way: nothing re-enables it");
        assert!(
            !motion.note_frame(ms(50)),
            "and it reports the latch exactly once"
        );
    }

    /// One animation per cell. A cell whose state churns replaces its own
    /// fade rather than stacking, which is what bounds how long a fade can
    /// run to its own duration.
    #[test]
    fn a_second_change_to_one_cell_replaces_rather_than_stacks() {
        let now = Instant::now();
        let mut motion = Motion::new(true);
        motion.observe(idle("%0"), now);
        for step in 1..40 {
            let ink = if step % 2 == 0 {
                StatusInk::State(AgentState::Working)
            } else {
                StatusInk::State(AgentState::Idle)
            };
            motion.observe(seen("%0", ink, None), now + ms(step * 4));
        }
        assert_eq!(motion.anims.len(), 1, "one status cell, one animation");
        assert!(motion.tick(now + ms(40 * 4) + STATUS + MOTION_FRAME));
        assert_eq!(motion.deadline(), None);
    }
}
