//! Agent turn/availability state and the sensor fusion readout.
//!
//! Fusion exists for the rare blocked states, not steady-state accuracy
//! (validation amendment h): hooks and titles agree in steady state; the
//! screen sensor is what catches permission prompts, vendor modals, and
//! quota exhaustion, none of which emit a hook on any tested CLI.

use serde::{Deserialize, Serialize};

/// Fused verdict for one agent pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// No evidence yet, or sensors disagree without a safe resolution.
    Unknown,
    /// No turn is running. A statement about the turn ONLY: whether the
    /// composer is empty is the separate write-readiness question, and
    /// `Detection::write_ready` is the one place it is answered (rule 12).
    Idle,
    /// Composer holds staged text. Injection would concatenate; unsafe.
    IdleWithInput,
    /// A turn is running.
    Working,
    /// A vendor dialog owns the screen. Injection unsafe; dismissal is
    /// per-CLI manifest data, never a generic Enter/Escape (F3, F12).
    BlockedModal,
    /// Interactive permission/approval prompt. Screen-only on every tested
    /// CLI: no hook fires for it.
    BlockedPermission,
    /// Terminal state: vendor quota exhausted (F11). Passes liveness checks,
    /// emits no hook. Park the agent and alert the admin; never auto-retry.
    BlockedQuota,
    /// Pane process exited.
    Dead,
}

impl AgentState {
    /// True for states that should raise operator attention.
    pub fn is_blocked(self) -> bool {
        matches!(
            self,
            AgentState::BlockedModal | AgentState::BlockedPermission | AgentState::BlockedQuota
        )
    }

    /// The state glyph, one of the two encodings that carry meaning
    /// (GOALS). Every one of these measures ONE column and renders as
    /// text, never as a color emoji: the grid is strict, and a glyph the
    /// terminal draws double-wide or in its own emoji font breaks the
    /// column rhythm and cannot take the theme's color.
    ///
    /// blocked_quota was U+26D4 (no entry), which is East Asian Wide and
    /// defaults to emoji presentation. U+2298 (circled division slash) is
    /// one column, plain text, and the same geometric family as the
    /// circles above it. The warning sign stays: it is one column and does
    /// not default to emoji presentation.
    pub fn glyph(self) -> &'static str {
        match self {
            AgentState::Unknown => "?",
            AgentState::Idle => "○",
            AgentState::IdleWithInput => "◐",
            AgentState::Working => "●",
            AgentState::BlockedModal | AgentState::BlockedPermission => "⚠",
            AgentState::BlockedQuota => "⊘",
            AgentState::Dead => "✗",
        }
    }
}

/// The state cell's words: glyph, space, state name. "● working", "○ idle".
///
/// Every surface that shows a state shows these exact words: the CLI grids,
/// the stream, and the pane border chrome the daemon writes into tmux. It
/// lives here rather than in a rendering crate because the daemon has no
/// business linking a terminal UI to name a state, and a second spelling on
/// the borders would put a different word on the pane than on the grid
/// naming the same pane.
pub fn state_words(s: AgentState) -> String {
    format!("{} {s}", s.glyph())
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AgentState::Unknown => "unknown",
            AgentState::Idle => "idle",
            AgentState::IdleWithInput => "idle_with_input",
            AgentState::Working => "working",
            AgentState::BlockedModal => "blocked_modal",
            AgentState::BlockedPermission => "blocked_permission",
            AgentState::BlockedQuota => "blocked_quota",
            AgentState::Dead => "dead",
        };
        f.write_str(s)
    }
}

/// Which sensor produced a reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensor {
    /// Vendor hook event relayed via agent.state.report. Edge-triggered,
    /// high precision, incomplete coverage.
    Hook,
    /// #{pane_title}. Authoritative busy/idle on Claude only; the other
    /// tested CLIs publish a static string (F5).
    Title,
    /// %output activity from the control connection.
    Output,
    /// capture-pane bottom-region rules from the manifest. Consulted last,
    /// because it costs a capture the title tier can often avoid, but not
    /// a fallback: it is the only sensor that sees blocked states, and the
    /// only one that can see a composer, so write-readiness REQUIRES a
    /// positive clean-composer reading from it (rule 12).
    Screen,
}

/// One sensor's current opinion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorReading {
    pub sensor: Sensor,
    pub state: AgentState,
    /// Manifest rule id or event name that produced this reading.
    pub rule: String,
    /// Unix ms when the reading was taken.
    pub ts: u64,
}

/// Full fusion readout for one pane, exposed via pane.read source=detection.
/// Sensor disagreement is an observable state, not an internal error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detection {
    pub state: AgentState,
    pub readings: Vec<SensorReading>,
    /// True when live sensors disagree on the fused verdict.
    pub disagreement: bool,
    /// Rule/priority that won.
    pub decided_by: String,
    /// True when this verdict is a retained prior one, kept because a
    /// sensor read failed. The runtime state may still be the best answer
    /// available, but nothing here was observed just now, so it can never
    /// authorize a write (rule 12).
    #[serde(default)]
    pub stale: bool,
    /// The second answer, carried on the wire so a JSON consumer gets it
    /// without re-deriving policy. Always present, because an absent field
    /// cannot be told apart from an older daemon that never computed one.
    #[serde(default)]
    pub write_ready: bool,
    /// Why a write is refused, content-free, absent when it is allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_block: Option<String>,
}

/// What a pane's composer has proven since text was last seen staged in
/// it.
///
/// Runtime idleness and an empty composer are different facts (rule 12),
/// and so are an empty composer and a composer that was never holding
/// anything. A screen rule reads one frame: a pane holding somebody's
/// half-typed message can render as clean for a frame while it redraws,
/// or while the text sits somewhere the rule does not look. Admitting a
/// write on that frame pastes into a person's sentence.
///
/// So a pane that has been seen holding text stays refused until a TURN
/// proves the text left, which is the only positive evidence any vendor
/// gives. Nothing here is inferred from elapsed time or from a hook that
/// did not arrive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerHold {
    /// Nothing staged has been seen, or a turn has since consumed it.
    #[default]
    Clear,
    /// Text was seen staged, and nothing has proven it left.
    Staged,
    /// Text was seen staged while a turn was ALREADY running.
    ///
    /// That turn cannot be the one that consumes it: it started before
    /// the text existed. The distinction matters because a draft can go
    /// briefly invisible between frames while the same old `working`
    /// level persists, and promoting on that reading would let the old
    /// turn's end clear a draft it never saw.
    ///
    /// It becomes ordinary `Staged` once no sensor reads a turn running
    /// any more, so the NEXT start is a real one. A message-correlated
    /// receipt promotes it directly, because a payload Cyclops wrote and
    /// submitted is answered by the turn its own acknowledgement names.
    StagedDuringTurn,
    /// A turn started while text was staged: the agent took it. A bound
    /// exact lifecycle lifts only on the structurally matching turn key
    /// plus a clean composer. The timestamp remains for the screen lane
    /// and diagnostics; it never correlates an exact turn.
    TurnStarted { since_ms: u64 },
}

impl ComposerHold {
    /// Advance the hold on one fused verdict.
    ///
    /// Reads the READINGS, not the fused winner. The winner is one state
    /// chosen by priority, so a pane can win `idle` off a composer rule
    /// while another sensor is still reporting the turn that is running.
    /// Releasing on that is the false-idle class this hold exists to
    /// contain, so any live reading of `working` keeps it.
    ///
    /// `ended` is the daemon's answer about THIS turn, and its shape
    /// selects the lane. `None` is the screen lifecycle, used by every
    /// vendor that cannot name its turns. `Some(true)` means the vendor
    /// reported the exact TurnKey this hold is waiting on;
    /// `Some(false)` means it can and has not.
    ///
    /// The answer arrives already decided because correlating a turn is
    /// structural work over manifest-declared fields, and none of that
    /// belongs in a protocol type.
    pub fn advance(self, det: &Detection, ended: Option<bool>) -> ComposerHold {
        // A pane whose process is gone holds nothing for anyone; the next
        // occupant starts with no history.
        if det.state == AgentState::Dead {
            return ComposerHold::Clear;
        }
        // Text in the composer, from any sensor that can see one.
        if det.state == AgentState::IdleWithInput
            || det.reads(Sensor::Screen, AgentState::IdleWithInput)
        {
            return match self {
                // Already known to have been staged under a running
                // turn. Reading the text again does not change that.
                ComposerHold::StagedDuringTurn => self,
                _ if det.turn_running_at().is_some() => ComposerHold::StagedDuringTurn,
                _ => ComposerHold::Staged,
            };
        }
        if self == ComposerHold::Clear {
            return self;
        }
        // The turn that was already running when this text appeared is
        // not allowed to consume it, however many `working` frames it
        // spans. Once nothing reads a turn running, that turn is over and
        // the next start is a real one.
        if self == ComposerHold::StagedDuringTurn {
            return match det.turn_running_at() {
                Some(_) => self,
                None => ComposerHold::Staged,
            };
        }
        // A turn is running if ANY sensor says so, whoever won.
        if let Some(ts) = det.turn_running_at() {
            return match self {
                // The mark is the FIRST evidence of this turn, not the
                // latest: re-stamping it on every working frame would
                // push it past the turn-end edge that is meant to clear
                // it.
                ComposerHold::TurnStarted { since_ms } => ComposerHold::TurnStarted { since_ms },
                _ => ComposerHold::TurnStarted { since_ms: ts },
            };
        }
        let ComposerHold::TurnStarted { since_ms } = self else {
            return self;
        };
        // The turn is no longer running and the composer reads clean. On
        // an exact lifecycle that is not enough: the stored end must name
        // the TurnKey this hold carries.
        // The mark stays for the screen lane and for diagnostics; the
        // exact lane never compares times, because an end can be observed
        // before the start it belongs to.
        let _ = since_ms;
        let ended = ended.unwrap_or(det.state == AgentState::Idle);
        if ended && det.screen_says_clean() {
            return ComposerHold::Clear;
        }
        self
    }

    /// Is this hold still waiting for a turn to take the staged text?
    ///
    /// True for both staged shapes. They differ only in which turn is
    /// allowed to consume the text, not in whether one still has to.
    pub fn is_waiting(self) -> bool {
        matches!(self, ComposerHold::Staged | ComposerHold::StagedDuringTurn)
    }

    /// Does this hold refuse a write?
    pub fn refuses(self) -> bool {
        self != ComposerHold::Clear
    }
}

impl Detection {
    /// Refuse this verdict for a named reason, whatever the sensors said.
    ///
    /// The reason travels with the refusal because it is the only thing
    /// that tells an operator, or a waiting delivery, what would have to
    /// change. A refusal with no name is a pane that stopped working for
    /// reasons nobody can look up.
    pub fn refused(mut self, reason: &str) -> Detection {
        self.write_ready = false;
        self.write_block = Some(reason.to_string());
        self
    }

    /// Refuse because who is in the pane could not be read.
    ///
    /// Not the same as a pane with nobody in it. Every receipt is held
    /// against the admitted process, so an unreadable process table is
    /// doubt, and doubt is a refusal rather than a shrug.
    pub fn occupant_unprovable(self) -> Detection {
        self.refused("occupant_unprovable")
    }

    /// Stamp the final readiness verdict: the sensor policy, then the
    /// composer hold, then the pane's own mode. One writer, one answer,
    /// and every surface, the gate included, reads the stamped fields
    /// rather than re-deriving.
    pub fn stamped(mut self, in_mode: bool, hold: ComposerHold) -> Detection {
        if in_mode {
            // Copy-mode is the human reading their own scrollback. The
            // sensors can say whatever they like about the composer; the
            // pane is not theirs to write into right now.
            self.write_ready = false;
            self.write_block = Some("pane_in_mode".to_string());
            return self;
        }
        let mut det = self.with_write_block();
        if det.write_ready && hold.refuses() {
            det.write_ready = false;
            det.write_block = Some("composer_hold".to_string());
        }
        det
    }

    /// Does this sensor currently read this state?
    pub fn reads(&self, sensor: Sensor, state: AgentState) -> bool {
        self.readings
            .iter()
            .any(|r| r.sensor == sensor && r.state == state)
    }

    /// When the oldest live reading of a running turn was taken, if any
    /// sensor reports one.
    ///
    /// Any sensor counts. The fused winner is one state chosen by
    /// priority, so a composer rule can win `idle` while the title or a
    /// hook still reports the turn that is running, and a turn nobody
    /// won is still a turn.
    pub fn turn_running_at(&self) -> Option<u64> {
        self.readings
            .iter()
            .filter(|r| r.state == AgentState::Working)
            .map(|r| r.ts)
            .min()
    }

    /// Did a retained hook reading arrive after `since_ms`?
    ///
    /// This is an ordering query, not turn correlation. Exact lifecycle
    /// release uses a manifest-declared TurnKey in the daemon. Callers
    /// must not use this helper to authorize an exact turn.
    pub fn hook_turn_end_after(&self, since_ms: u64) -> bool {
        self.readings
            .iter()
            .any(|r| r.sensor == Sensor::Hook && r.state == AgentState::Idle && r.ts >= since_ms)
    }

    /// Did the sensor that can see a composer say, just now, that it is
    /// clean?
    ///
    /// The one definition of that evidence, used by the readiness rule
    /// and by the hold that releases on it. A title or a hook reports a
    /// turn boundary, which is a different fact.
    pub fn screen_says_clean(&self) -> bool {
        self.readings
            .iter()
            .any(|r| r.sensor == Sensor::Screen && r.state == AgentState::Idle)
    }

    fn with_write_block(mut self) -> Detection {
        match self.base_write_ready() {
            Ok(()) => {
                self.write_ready = true;
                self.write_block = None;
            }
            Err(reason) => {
                self.write_ready = false;
                self.write_block = Some(reason.to_string());
            }
        }
        self
    }

    /// The sensor half of write-readiness.
    ///
    /// Deliberately not public: it cannot see pane mode or any temporal
    /// hold, so a caller consulting it directly would be answering a
    /// narrower question than the one delivery asks and could overwrite
    /// an authoritative refusal with a cheerful one. Fusion stamps the
    /// final verdict onto the Detection; everyone else reads that.
    fn base_write_ready(&self) -> Result<(), &'static str> {
        if self.state != AgentState::Idle {
            return Err("not_idle");
        }
        // A retained verdict is doubt wearing the last known answer: the
        // capture that was supposed to look at the composer failed.
        if self.stale {
            return Err("stale_screen_evidence");
        }
        if self.disagreement {
            return Err("sensor_disagreement");
        }
        // A hook edge reports that generation stopped. It cannot see the
        // composer, so when fusion had to fall back to one because the
        // screen rules resolved to nothing, there is no clean-input
        // evidence at all: that is the shape a long staged payload makes.
        if self.decided_by.starts_with("hook:") {
            return Err("hook_derived_idle");
        }
        // Only the screen sensor can see a composer. A title or hook edge
        // reports a turn boundary, which is a different fact: the pane can
        // be between turns with a person's half-written message sitting in
        // it. So the write needs the screen saying, positively and just
        // now, that the composer is clean. A manifest that cannot produce
        // that reading cannot authorize a write; that is a gap in the
        // manifest, not a licence to guess.
        if !self.screen_says_clean() {
            return Err("no_clean_composer_evidence");
        }
        let conflict = self.readings.iter().any(|r| {
            matches!(
                r.state,
                AgentState::Working
                    | AgentState::IdleWithInput
                    | AgentState::BlockedModal
                    | AgentState::BlockedPermission
                    | AgentState::BlockedQuota
                    | AgentState::Unknown
            )
        });
        if conflict {
            return Err("conflicting_evidence");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_names_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&AgentState::BlockedQuota).unwrap(),
            "\"blocked_quota\""
        );
        assert_eq!(
            serde_json::from_str::<AgentState>("\"idle_with_input\"").unwrap(),
            AgentState::IdleWithInput
        );
    }

    #[test]
    fn blocked_states_are_the_blocked_ones() {
        // Runtime state no longer answers "may I write". It could not:
        // idle says no turn is running, which a pane holding somebody's
        // half-typed message also says. The answer lives on Detection,
        // stamped once, with the evidence behind it.
        // is_blocked still has to name exactly the three blocked states,
        // so both directions are asserted: dropping a variant from the
        // match and adding a non-blocked one both have to fail here.
        for s in [
            AgentState::BlockedModal,
            AgentState::BlockedPermission,
            AgentState::BlockedQuota,
        ] {
            assert!(s.is_blocked(), "{s} is a blocked state");
        }
        for s in [
            AgentState::Unknown,
            AgentState::Idle,
            AgentState::IdleWithInput,
            AgentState::Working,
            AgentState::Dead,
        ] {
            assert!(!s.is_blocked(), "{s} is not a blocked state");
        }
    }
}

#[cfg(test)]
mod write_ready_tests {
    use super::*;

    fn reading(sensor: Sensor, state: AgentState) -> SensorReading {
        SensorReading {
            sensor,
            state,
            rule: "r".into(),
            ts: 0,
        }
    }

    fn det(state: AgentState, readings: Vec<SensorReading>, disagreement: bool) -> Detection {
        Detection {
            state,
            readings,
            disagreement,
            decided_by: "d".into(),
            stale: false,
            write_ready: false,
            write_block: None,
        }
    }

    /// The case that made this rule necessary: a turn-end hook maps to
    /// idle, fusion adopts it because the screen rules read unknown, and
    /// the composer is actually holding a long staged payload. Before rule
    /// 12 the gate proceeded here and pasted over it.
    #[test]
    fn hook_idle_over_unknown_screen_is_not_write_ready() {
        let mut d = det(
            AgentState::Idle,
            vec![
                reading(Sensor::Hook, AgentState::Idle),
                reading(Sensor::Screen, AgentState::Unknown),
            ],
            false,
        );
        d.decided_by = "hook:Stop".into();
        // The hook stood in for a screen that read nothing, which is the
        // exact shape of a composer holding a long staged payload.
        assert_eq!(d.base_write_ready(), Err("hook_derived_idle"));
    }

    /// A hook edge with no screen reading at all cannot authorize a write:
    /// nothing looked at the composer.
    #[test]
    fn hook_idle_alone_is_not_write_ready() {
        let mut d = det(
            AgentState::Idle,
            vec![reading(Sensor::Hook, AgentState::Idle)],
            false,
        );
        d.decided_by = "hook:Stop".into();
        assert_eq!(d.base_write_ready(), Err("hook_derived_idle"));
    }

    /// The reverse race: screen rules say idle while a live hook says
    /// working. Fusion records disagreement and keeps the rule verdict;
    /// a write must not ride on a contested verdict.
    #[test]
    fn disagreement_is_never_write_ready() {
        let d = det(
            AgentState::Idle,
            vec![
                reading(Sensor::Screen, AgentState::Idle),
                reading(Sensor::Hook, AgentState::Working),
            ],
            true,
        );
        assert_eq!(d.base_write_ready(), Err("sensor_disagreement"));
    }

    /// Staged input is the whole point of the rule.
    #[test]
    fn staged_input_is_never_write_ready() {
        let d = det(
            AgentState::IdleWithInput,
            vec![reading(Sensor::Screen, AgentState::IdleWithInput)],
            false,
        );
        assert_eq!(d.base_write_ready(), Err("not_idle"));
    }

    /// The one shape that admits a write: the sensor that sees the
    /// composer says it is empty, and nothing live contradicts it.
    #[test]
    fn positive_clean_screen_evidence_is_write_ready() {
        let d = det(
            AgentState::Idle,
            vec![
                reading(Sensor::Screen, AgentState::Idle),
                reading(Sensor::Hook, AgentState::Idle),
            ],
            false,
        );
        assert_eq!(d.base_write_ready(), Ok(()));
    }

    /// Fusion keeps the prior verdict when a forced capture fails, which
    /// is right for reporting state and wrong for authorizing a write. The
    /// retained reading looks clean because it WAS clean, seconds ago,
    /// before the read that failed.
    #[test]
    fn a_retained_verdict_is_never_write_ready() {
        let mut d = det(
            AgentState::Idle,
            vec![reading(Sensor::Screen, AgentState::Idle)],
            false,
        );
        assert_eq!(d.base_write_ready(), Ok(()));
        d.stale = true;
        assert_eq!(d.base_write_ready(), Err("stale_screen_evidence"));
    }

    /// Copy-mode is the human reading their own scrollback, and it
    /// outranks whatever the sensors think of the composer. The stamp is
    /// where the two are combined, once, so no surface can answer this
    /// question differently from the gate.
    #[test]
    fn pane_mode_refuses_however_clean_the_screen_is() {
        let clean = det(
            AgentState::Idle,
            vec![reading(Sensor::Screen, AgentState::Idle)],
            false,
        );
        assert_eq!(clean.base_write_ready(), Ok(()));
        let stamped = clean.clone().stamped(true, ComposerHold::Clear);
        assert!(!stamped.write_ready);
        assert_eq!(stamped.write_block.as_deref(), Some("pane_in_mode"));
        let stamped = clean.clone().stamped(false, ComposerHold::Clear);
        assert!(stamped.write_ready);
        assert_eq!(stamped.write_block, None);
    }

    /// One frame of a clean composer is not proof the composer is empty.
    ///
    /// A pane holding somebody's half-typed message can render clean
    /// while it redraws, or while the text sits somewhere the screen rule
    /// does not look. The sensor rule cannot tell that frame from a
    /// genuinely empty composer, which is why the hold is a separate
    /// answer and why it outranks a clean reading.
    #[test]
    fn a_pane_that_was_holding_text_refuses_a_clean_frame() {
        let clean = det(
            AgentState::Idle,
            vec![reading(Sensor::Screen, AgentState::Idle)],
            false,
        );
        for hold in [
            ComposerHold::Staged,
            ComposerHold::TurnStarted { since_ms: 1 },
        ] {
            let stamped = clean.clone().stamped(false, hold);
            assert!(!stamped.write_ready, "{hold:?} admitted a write");
            assert_eq!(stamped.write_block.as_deref(), Some("composer_hold"));
        }
    }

    /// The hold moves on positive edges only: text seen staged, and a
    /// turn running. Silence moves nothing, and nothing here reads a
    /// clock.
    ///
    /// This is the no-hook vendor, where the screen is the only sensor
    /// that can end a turn.
    #[test]
    fn the_hold_releases_only_on_a_completed_turn() {
        let clean = det(
            AgentState::Idle,
            vec![reading(Sensor::Screen, AgentState::Idle)],
            false,
        );
        let staged = det(
            AgentState::IdleWithInput,
            vec![reading(Sensor::Screen, AgentState::IdleWithInput)],
            false,
        );
        let working = det(
            AgentState::Working,
            vec![reading(Sensor::Screen, AgentState::Working)],
            false,
        );
        // Somebody's text is in the composer.
        let h = ComposerHold::Clear.advance(&staged, None);
        assert_eq!(h, ComposerHold::Staged);
        // It stops being visible. That is not evidence it left, so a
        // clean frame, however many times it repeats, changes nothing.
        let h = h
            .advance(&clean, None)
            .advance(&clean, None)
            .advance(&clean, None);
        assert_eq!(h, ComposerHold::Staged);
        assert!(h.refuses());
        // A turn starts: the agent took the text.
        let h = h.advance(&working, None);
        assert!(matches!(h, ComposerHold::TurnStarted { .. }));
        // The turn ending is still not enough on its own. Only a turn
        // that ends WITH a clean composer releases it, and the clean
        // half has to come from the screen.
        let title_only = det(
            AgentState::Idle,
            vec![reading(Sensor::Title, AgentState::Idle)],
            false,
        );
        assert_eq!(h.advance(&title_only, None), h);
        assert_eq!(h.advance(&clean, None), ComposerHold::Clear);
    }

    /// The fused winner is one state chosen by priority, so a composer
    /// rule can win `idle` while another sensor still reports the turn
    /// that is running. Releasing on that winner is the false-idle class
    /// the hold exists to contain.
    #[test]
    fn a_sensor_still_reporting_the_turn_keeps_the_hold() {
        let mut mid_turn = det(
            AgentState::Idle,
            vec![
                reading(Sensor::Screen, AgentState::Idle),
                reading(Sensor::Title, AgentState::Working),
            ],
            false,
        );
        for r in &mut mid_turn.readings {
            r.ts = 50;
        }
        let held = ComposerHold::TurnStarted { since_ms: 10 };
        assert_eq!(
            held.advance(&mid_turn, None),
            held,
            "a clean frame released a turn another sensor says is running"
        );
    }

    /// Turn-start evidence does not have to come from a sensor.
    ///
    /// A turn shorter than the gap between two captures paints its
    /// spinner and finishes with nobody looking. A hold that only
    /// advanced on an observed `working` frame would then wait for a turn
    /// that already happened, which deadlocks the recipient. Delivery
    /// promotes the hold on a RECEIPT, which is proof the composer was
    /// consumed, and the release path has to honour that mark without
    /// ever having seen `working`. Sending the submit key is NOT that proof: tmux
    /// accepting a keystroke says nothing about what the vendor did with
    /// it.
    #[test]
    fn a_turn_nobody_watched_still_ends() {
        let clean = det(
            AgentState::Idle,
            vec![reading(Sensor::Screen, AgentState::Idle)],
            false,
        );
        let latched = ComposerHold::TurnStarted { since_ms: 1 };
        assert_eq!(latched.advance(&clean, None), ComposerHold::Clear);
    }

    /// `Clear` is absorbing, which is why nothing may reach it early.
    ///
    /// The shipped ACK event is `UserPromptSubmit`: the vendor saying it
    /// has taken the prompt, which is a turn START. Clearing on it would
    /// end a turn that is still running, and no later `working` frame
    /// could undo that, because a cleared hold stays cleared until text
    /// is seen staged again.
    #[test]
    fn clear_is_absorbing_so_a_running_turn_cannot_restore_it() {
        let working = det(
            AgentState::Working,
            vec![reading(Sensor::Screen, AgentState::Working)],
            false,
        );
        assert_eq!(
            ComposerHold::Clear.advance(&working, None),
            ComposerHold::Clear,
            "a running turn must not resurrect a hold nobody is holding"
        );
        // Which is the whole reason a correlated turn START binds the
        // turn rather than clearing the hold.
        assert!(matches!(
            ComposerHold::Staged.advance(&working, None),
            ComposerHold::TurnStarted { .. }
        ));
    }

    /// A turn that was already running cannot consume text it never saw.
    ///
    /// The bug this pins: a person types while the agent is mid-turn, so
    /// the hold goes to `Staged`. The next capture misses the draft while
    /// the SAME old `working` level is still being reported, and the hold
    /// was promoted to `TurnStarted` off that stale level. The old turn
    /// then ended, the composer read clean for a frame, and the hold
    /// cleared over a draft nothing had consumed.
    ///
    /// Timestamps cannot separate these: a sampled `working` level from a
    /// turn already in flight looks exactly like the first frame of a new
    /// one. What separates them is whether the turn was already running
    /// when the text appeared, which is what the hold now remembers.
    #[test]
    fn a_turn_already_running_cannot_consume_text_it_never_saw() {
        // The draft appears while a turn is in flight.
        let typed_mid_turn = det(
            AgentState::Working,
            vec![
                reading(Sensor::Screen, AgentState::IdleWithInput),
                reading(Sensor::Hook, AgentState::Working),
            ],
            false,
        );
        // The next frame cannot see the draft; the same old turn is still
        // being reported.
        let draft_invisible = det(
            AgentState::Working,
            vec![reading(Sensor::Hook, AgentState::Working)],
            false,
        );
        // That turn ends and the composer reads clean.
        let old_turn_ends = det(
            AgentState::Idle,
            vec![reading(Sensor::Screen, AgentState::Idle)],
            false,
        );
        // Later, a distinct turn starts.
        let new_turn = det(
            AgentState::Working,
            vec![reading(Sensor::Screen, AgentState::Working)],
            false,
        );

        let hold = ComposerHold::Clear.advance(&typed_mid_turn, None);
        assert_eq!(
            hold,
            ComposerHold::StagedDuringTurn,
            "text staged under a running turn is remembered as such"
        );

        let hold = hold.advance(&draft_invisible, None);
        assert_eq!(
            hold,
            ComposerHold::StagedDuringTurn,
            "a stale working level is not a start edge"
        );

        let hold = hold.advance(&old_turn_ends, None);
        assert_eq!(
            hold,
            ComposerHold::Staged,
            "the old turn ending releases nothing; it only makes the next start a real one"
        );
        assert!(hold.refuses(), "the draft is still in there");

        // Only a turn that began after the text can take it.
        let hold = hold.advance(&new_turn, None);
        assert!(matches!(hold, ComposerHold::TurnStarted { .. }));
        assert_eq!(hold.advance(&old_turn_ends, None), ComposerHold::Clear);
    }

    /// A vendor that can name its turns is not released by the screen.
    ///
    /// Its end is a fact it reports about a specific turn, so a clean
    /// composer proves only that the composer is clean. Until that turn's
    /// own end arrives the hold stands, however idle the pane looks.
    #[test]
    fn a_vendor_that_names_its_turns_waits_for_its_own_end() {
        let clean = det(
            AgentState::Idle,
            vec![reading(Sensor::Screen, AgentState::Idle)],
            false,
        );
        let held = ComposerHold::TurnStarted { since_ms: 50 };
        assert_eq!(
            held.advance(&clean, Some(false)),
            held,
            "the screen ended a turn the vendor reports itself"
        );
        assert_eq!(held.advance(&clean, Some(true)), ComposerHold::Clear);
    }

    /// Even its own end needs a clean composer: the turn ending says the
    /// agent stopped, not that the composer is empty.
    #[test]
    fn an_exact_end_still_needs_a_clean_composer() {
        let staged_screen = det(
            AgentState::Idle,
            vec![reading(Sensor::Screen, AgentState::IdleWithInput)],
            false,
        );
        let held = ComposerHold::TurnStarted { since_ms: 50 };
        assert_eq!(
            held.advance(&staged_screen, Some(true)),
            ComposerHold::Staged
        );
    }

    /// A vendor whose hooks are dead cannot be waited on forever.
    ///
    /// Hook authority is the caller's decision (fusion requires a live
    /// hook reading before claiming it), and this is the contract it
    /// rests on: with authority off, the same evidence releases. If that
    /// were not true, a fresh install whose hooks are not wired yet would
    /// hold every pane it ever saw text in, permanently.
    #[test]
    fn a_vendor_that_reports_no_hooks_releases_on_the_screen() {
        let clean = det(
            AgentState::Idle,
            vec![reading(Sensor::Screen, AgentState::Idle)],
            false,
        );
        let held = ComposerHold::TurnStarted { since_ms: 0 };
        assert_eq!(
            held.advance(&clean, Some(false)),
            held,
            "a vendor that names its turns waits for its own end"
        );
        assert_eq!(
            held.advance(&clean, None),
            ComposerHold::Clear,
            "with no way to name a turn, the screen is the whole contract"
        );
    }

    /// A pane whose process is gone holds nothing for whoever comes next.
    #[test]
    fn death_clears_the_hold() {
        let dead = det(AgentState::Dead, vec![], false);
        assert_eq!(
            ComposerHold::Staged.advance(&dead, None),
            ComposerHold::Clear
        );
    }
}
