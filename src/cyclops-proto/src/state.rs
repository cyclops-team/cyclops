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
    /// Composer ready, safe to inject.
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
    /// True when the delivery pipeline may paste into this pane.
    pub fn safe_to_inject(self) -> bool {
        matches!(self, AgentState::Idle)
    }

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
    /// capture-pane bottom-region rules from the manifest. Evidence of last
    /// resort, and the only sensor that sees blocked states.
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

impl Detection {
    /// May a payload be written into this pane's composer right now?
    ///
    /// Runtime idleness and terminal write-readiness are different
    /// questions (INVARIANTS rule 12). A turn-end hook proves generation
    /// stopped; it cannot prove the composer is empty, and a pane whose
    /// screen rules read `unknown` while a hook says `idle` is exactly the
    /// shape of a long staged payload waiting to be pasted over.
    ///
    /// A write therefore needs positive, current, clean-composer evidence
    /// from the sensor that can actually see the composer, and no live
    /// reading that contradicts it. `Err` carries the reason for the gate
    /// ledger line.
    /// Stamp [`Self::write_block`] from the rule, for surfaces that
    /// serialize a detection. Keeps one owner: the field is a rendering of
    /// `write_ready`, never a second opinion.
    pub fn with_write_block(mut self) -> Detection {
        match self.write_ready() {
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

    pub fn write_ready(&self) -> Result<(), &'static str> {
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
        let screen_clean = self
            .readings
            .iter()
            .any(|r| r.sensor == Sensor::Screen && r.state == AgentState::Idle);
        if !screen_clean {
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
    fn only_idle_is_injectable() {
        for s in [
            AgentState::Unknown,
            AgentState::IdleWithInput,
            AgentState::Working,
            AgentState::BlockedModal,
            AgentState::BlockedPermission,
            AgentState::BlockedQuota,
            AgentState::Dead,
        ] {
            assert!(!s.safe_to_inject(), "{s} must not be injectable");
        }
        assert!(AgentState::Idle.safe_to_inject());
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
        assert_eq!(d.write_ready(), Err("hook_derived_idle"));
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
        assert_eq!(d.write_ready(), Err("hook_derived_idle"));
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
        assert_eq!(d.write_ready(), Err("sensor_disagreement"));
    }

    /// Staged input is the whole point of the rule.
    #[test]
    fn staged_input_is_never_write_ready() {
        let d = det(
            AgentState::IdleWithInput,
            vec![reading(Sensor::Screen, AgentState::IdleWithInput)],
            false,
        );
        assert_eq!(d.write_ready(), Err("not_idle"));
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
        assert_eq!(d.write_ready(), Ok(()));
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
        assert_eq!(d.write_ready(), Ok(()));
        d.stale = true;
        assert_eq!(d.write_ready(), Err("stale_screen_evidence"));
    }
}
