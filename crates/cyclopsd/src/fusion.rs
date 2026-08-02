//! Sensor fusion: title tier plus screen tier over manifest rules, with
//! output activity as a recompute trigger (never a verdict), and the hook
//! sensor fed by agent.state.report (M1).
//!
//! Tier semantics mirror `Manifest::evaluate`: rules are already sorted by
//! priority, the first match in a region class wins that tier, and the
//! fused verdict is whichever tier winner sits earlier in that same order.
//! When both tiers produced a rule and their states differ, the verdict
//! still goes to the higher-priority rule but the disagreement is exposed
//! on the Detection (GOALS: observable, not an error).
//!
//! Screen capture is evidence of last resort (amendment h): when a
//! pane_title rule alone decides, capture-pane is skipped entirely. An
//! explicit `pane.read source=detection` forces the full sensor set,
//! which is the reconcile-on-doubt path.

use std::collections::BTreeMap;
use std::sync::Arc;

use cyclops_manifest::{CompiledRule, Manifest, Region};
use cyclops_proto::{AgentState, Detection, Sensor, SensorReading};
use cyclops_tmux::SessionWatcher;
use tracing::debug;

use crate::{unix_ms, DetEntry, Inner};

/// Bind a manifest to a pane by its foreground command. Deterministic:
/// manifests iterate in id order. The explicit adoption registry replaces
/// this in M1; until then process name is the only binding signal.
pub(crate) fn bind_manifest<'a>(
    manifests: &'a BTreeMap<String, Manifest>,
    current_command: &str,
) -> Option<&'a Manifest> {
    manifests
        .values()
        .find(|m| m.agent.process_names.iter().any(|p| p == current_command))
}

/// Highest-priority pane_title rule matching the title.
pub(crate) fn title_winner<'m>(m: &'m Manifest, title: &str) -> Option<&'m CompiledRule> {
    m.rules
        .iter()
        .find(|r| r.region == Region::PaneTitle && r.matches(title, &[title]))
}

/// Highest-priority screen-region rule matching the capture. Region
/// slicing matches `Manifest::evaluate`: bottom N non-empty lines,
/// restored to top-down order.
pub(crate) fn screen_winner<'m>(m: &'m Manifest, screen: &str) -> Option<&'m CompiledRule> {
    let non_empty: Vec<&str> = screen
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .collect();
    m.rules.iter().find(|r| match r.region {
        Region::PaneTitle => false,
        Region::BottomNonEmptyLines(n) => {
            let mut sel: Vec<&str> = non_empty.iter().take(n).copied().collect();
            sel.reverse();
            r.matches(&sel.join("\n"), &sel)
        }
    })
}

/// Fuse the tier winners into a Detection. Both readings are kept whenever
/// both tiers fired, whatever the verdict.
pub(crate) fn fuse(
    m: &Manifest,
    title: Option<&CompiledRule>,
    screen: Option<&CompiledRule>,
    ts: u64,
) -> Detection {
    let mut readings = Vec::new();
    if let Some(r) = title {
        readings.push(SensorReading {
            sensor: Sensor::Title,
            state: r.state,
            rule: r.id.clone(),
            ts,
        });
    }
    if let Some(r) = screen {
        readings.push(SensorReading {
            sensor: Sensor::Screen,
            state: r.state,
            rule: r.id.clone(),
            ts,
        });
    }
    // First rule in priority order that one of the tiers selected. Compared
    // by address: both winners are references into m.rules.
    let winner = m.rules.iter().find(|r| {
        let rp: *const CompiledRule = *r;
        title.is_some_and(|t| std::ptr::eq(rp, t)) || screen.is_some_and(|s| std::ptr::eq(rp, s))
    });
    match winner {
        Some(w) => Detection {
            state: w.state,
            disagreement: matches!((title, screen), (Some(t), Some(s)) if t.state != s.state),
            decided_by: w.id.clone(),
            readings,
        },
        None => Detection {
            state: AgentState::Unknown,
            readings,
            disagreement: false,
            decided_by: "no_rule".into(),
        },
    }
}

/// Recompute one pane's Detection, update the cache, and emit a "state"
/// event when the fused state changed. `force_screen` runs the full sensor
/// set even when a title rule alone would decide (pane.read detection).
/// Returns None when the pane is gone from the table.
pub(crate) async fn recompute_pane(
    inner: &Arc<Inner>,
    watcher: &SessionWatcher,
    pane_id: &str,
    force_screen: bool,
    cause: &str,
) -> Option<Detection> {
    let Some(row) = watcher.pane(pane_id) else {
        inner
            .detections
            .lock()
            .expect("detections lock")
            .remove(pane_id);
        return None;
    };
    let manifest = bind_manifest(&inner.manifests, &row.current_command);
    let manifest_id = manifest.map(|m| m.agent.id.clone());
    let ts = unix_ms();

    if !row.dead && row.in_mode {
        // Copy-mode and friends gate delivery in M1; they are not agent
        // states. Keep the prior verdict; status exposes in_mode per row.
        let mut map = inner.detections.lock().expect("detections lock");
        let det = match map.get_mut(pane_id) {
            Some(e) => {
                e.manifest = manifest_id;
                e.detection.clone()
            }
            None => {
                let det = Detection {
                    state: AgentState::Unknown,
                    readings: Vec::new(),
                    disagreement: false,
                    decided_by: "pane_in_mode".into(),
                };
                map.insert(
                    pane_id.to_string(),
                    DetEntry {
                        detection: det.clone(),
                        manifest: manifest_id,
                    },
                );
                det
            }
        };
        return Some(det);
    }

    let mut detection = if row.dead {
        Detection {
            state: AgentState::Dead,
            readings: Vec::new(),
            disagreement: false,
            decided_by: "pane_dead".into(),
        }
    } else if let Some(m) = manifest {
        let t_rule = title_winner(m, &row.title);
        let need_screen = force_screen || t_rule.is_none();
        let mut capture_failed = false;
        let screen = if need_screen {
            match watcher.client().capture_pane(pane_id).await {
                Ok(s) => Some(s),
                Err(e) => {
                    // Sensor failure is doubt, not evidence: keep the prior
                    // verdict rather than flipping state on a broken read.
                    debug!(pane = pane_id, error = %e, "capture failed; keeping prior state");
                    let prior = inner
                        .detections
                        .lock()
                        .expect("detections lock")
                        .get(pane_id)
                        .map(|entry| entry.detection.clone());
                    if let Some(p) = prior {
                        return Some(p);
                    }
                    capture_failed = true;
                    None
                }
            }
        } else {
            None
        };
        let s_rule = screen.as_deref().and_then(|s| screen_winner(m, s));
        let mut det = fuse(m, t_rule, s_rule, ts);
        // No prior to fall back on and the screen sensor errored: the rule
        // set was never fully consulted, and the record must not claim it
        // was (GOALS: the record never lies).
        if capture_failed && det.decided_by == "no_rule" {
            det.decided_by = "sensor_error".into();
        }
        det
    } else {
        Detection {
            state: AgentState::Unknown,
            readings: Vec::new(),
            disagreement: false,
            decided_by: "no_manifest".into(),
        }
    };

    // Hook sensor (agent.state.report): high-precision edges, incomplete
    // coverage. Rules keep the verdict when they produced one; the hook
    // decides only where rules see nothing, and a live disagreement stays
    // observable either way. Blocked states always come from rules, since
    // no tested CLI hooks its modals or quota (amendment h).
    if !row.dead {
        let hook = inner
            .hook_readings
            .lock()
            .expect("hook readings lock")
            .get(pane_id)
            .cloned();
        if let Some(reading) = hook {
            let hook_state = reading.state;
            let hook_rule = reading.rule.clone();
            detection.readings.push(reading);
            if detection.state == AgentState::Unknown {
                detection.state = hook_state;
                detection.decided_by = format!("hook:{hook_rule}");
            } else if hook_state != detection.state {
                detection.disagreement = true;
            }
        }
    }

    let prior = {
        let mut map = inner.detections.lock().expect("detections lock");
        let prior = map.get(pane_id).map(|e| e.detection.state);
        map.insert(
            pane_id.to_string(),
            DetEntry {
                detection: detection.clone(),
                manifest: manifest_id,
            },
        );
        prior
    };
    // First sight of a pane that reads Unknown is baseline, not a change.
    let changed = prior != Some(detection.state)
        && !(prior.is_none() && detection.state == AgentState::Unknown);
    if changed {
        debug!(
            pane = pane_id,
            state = %detection.state,
            prior = ?prior,
            cause,
            "fused state changed"
        );
        inner.emit_state(watcher.session(), pane_id, &detection, prior, cause);
    }
    Some(detection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const FIXTURE: &str = r#"
[agent]
id = "bash"
display_name = "Bash fixture"
process_names = ["bash"]

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^IDLE']

[[rule]]
id = "screen_busy"
state = "working"
priority = 800
region = "bottom_non_empty_lines(3)"
line_regex = ['^FIXPROMPT']
"#;

    fn manifest() -> Manifest {
        Manifest::parse(FIXTURE, Path::new("bash.toml")).unwrap()
    }

    #[test]
    fn binding_is_by_process_name_in_id_order() {
        let mut map = BTreeMap::new();
        map.insert("bash".to_string(), manifest());
        assert_eq!(
            bind_manifest(&map, "bash").map(|m| m.agent.id.as_str()),
            Some("bash")
        );
        assert!(bind_manifest(&map, "vim").is_none());
    }

    #[test]
    fn tier_winners() {
        let m = manifest();
        assert_eq!(
            title_winner(&m, "IDLE ready").map(|r| r.id.as_str()),
            Some("title_idle")
        );
        assert!(title_winner(&m, "mac").is_none());
        assert_eq!(
            screen_winner(&m, "junk\nFIXPROMPT ").map(|r| r.id.as_str()),
            Some("screen_busy")
        );
        assert!(screen_winner(&m, "nothing here").is_none());
    }

    #[test]
    fn disagreement_takes_higher_priority_and_keeps_both_readings() {
        let m = manifest();
        let t = title_winner(&m, "IDLE ready");
        let s = screen_winner(&m, "FIXPROMPT ");
        let d = fuse(&m, t, s, 1);
        assert_eq!(d.state, AgentState::Idle);
        assert_eq!(d.decided_by, "title_idle");
        assert!(d.disagreement);
        assert_eq!(d.readings.len(), 2);
        assert_eq!(d.readings[0].sensor, Sensor::Title);
        assert_eq!(d.readings[0].rule, "title_idle");
        assert_eq!(d.readings[1].sensor, Sensor::Screen);
        assert_eq!(d.readings[1].rule, "screen_busy");
    }

    #[test]
    fn single_tier_is_no_disagreement() {
        let m = manifest();
        let s = screen_winner(&m, "FIXPROMPT ");
        let d = fuse(&m, None, s, 1);
        assert_eq!(d.state, AgentState::Working);
        assert_eq!(d.decided_by, "screen_busy");
        assert!(!d.disagreement);
        assert_eq!(d.readings.len(), 1);
    }

    #[test]
    fn no_rule_is_unknown() {
        let m = manifest();
        let d = fuse(&m, None, None, 1);
        assert_eq!(d.state, AgentState::Unknown);
        assert_eq!(d.decided_by, "no_rule");
        assert!(d.readings.is_empty());
    }
}
