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
    /// A turn started while text was staged: the agent took it. The hold
    /// lifts on the first clean-composer reading after the turn ends.
    TurnStarted,
}

impl ComposerHold {
    /// Advance the hold on one fused verdict.
    ///
    /// Only the two positive edges move it: seeing text staged, and a
    /// turn running. A pane that goes quiet moves nothing, which is the
    /// whole point.
    pub fn advance(self, det: &Detection) -> ComposerHold {
        match det.state {
            // A pane whose process is gone holds nothing for anyone; the
            // next occupant starts with no history.
            AgentState::Dead => ComposerHold::Clear,
            AgentState::IdleWithInput => ComposerHold::Staged,
            AgentState::Working if self != ComposerHold::Clear => ComposerHold::TurnStarted,
            AgentState::Idle if self == ComposerHold::TurnStarted && det.screen_says_clean() => {
                ComposerHold::Clear
            }
            _ => self,
        }
    }

    /// Does this hold refuse a write?
    pub fn refuses(self) -> bool {
        self != ComposerHold::Clear
    }
}

impl Detection {
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
        for hold in [ComposerHold::Staged, ComposerHold::TurnStarted] {
            let stamped = clean.clone().stamped(false, hold);
            assert!(!stamped.write_ready, "{hold:?} admitted a write");
            assert_eq!(stamped.write_block.as_deref(), Some("composer_hold"));
        }
    }

    /// The hold moves on positive edges only: text seen staged, and a
    /// turn running. Silence moves nothing, and nothing here reads a
    /// clock.
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
        let h = ComposerHold::Clear.advance(&staged);
        assert_eq!(h, ComposerHold::Staged);
        // It stops being visible. That is not evidence it left, so a
        // clean frame, however many times it repeats, changes nothing.
        let h = h.advance(&clean).advance(&clean).advance(&clean);
        assert_eq!(h, ComposerHold::Staged);
        assert!(h.refuses());
        // A turn starts: the agent took the text.
        let h = h.advance(&working);
        assert_eq!(h, ComposerHold::TurnStarted);
        // The turn ending is still not enough on its own. Only a turn
        // that ends WITH a clean composer releases it, and the clean
        // half has to come from the screen.
        let title_only = det(
            AgentState::Idle,
            vec![reading(Sensor::Title, AgentState::Idle)],
            false,
        );
        assert_eq!(h.advance(&title_only), ComposerHold::TurnStarted);
        assert_eq!(h.advance(&clean), ComposerHold::Clear);
    }

    /// A pane whose process is gone holds nothing for whoever comes next.
    #[test]
    fn death_clears_the_hold() {
        let dead = det(AgentState::Dead, vec![], false);
        assert_eq!(ComposerHold::Staged.advance(&dead), ComposerHold::Clear);
    }
}
