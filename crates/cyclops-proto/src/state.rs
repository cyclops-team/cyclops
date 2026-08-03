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
