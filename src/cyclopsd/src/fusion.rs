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

use cyclops_manifest::{strip_csi, CompiledRule, Manifest, Region};
use cyclops_proto::{AgentState, Detection, Sensor, SensorReading};
use cyclops_tmux::{PaneRow, SessionWatcher, TmuxError};
use tracing::debug;

use crate::{unix_ms, DetEntry, Inner};

/// A hook reading older than this is spent: it can no longer decide fused
/// state on its own (a stale edge must not pin a verdict forever). Checked
/// only when a recompute runs anyway; no timer ages anything.
const HOOK_READING_TTL_MS: u64 = 300_000;
/// Consecutive rules-tier verdicts contradicting the hook reading before
/// the reading is invalidated.
const HOOK_DISAGREE_LIMIT: u32 = 3;

/// Stored hook sensor state per pane: the reading plus how many deciding
/// rules-tier recomputes have contradicted it in a row.
pub(crate) struct HookEntry {
    pub(crate) reading: SensorReading,
    pub(crate) disagreements: u32,
}

impl HookEntry {
    pub(crate) fn new(reading: SensorReading) -> HookEntry {
        HookEntry {
            reading,
            disagreements: 0,
        }
    }
}

/// Bind a manifest to a pane by its foreground command. Deterministic:
/// manifests iterate in id order.
pub(crate) fn bind_manifest<'a>(
    manifests: &'a BTreeMap<String, Manifest>,
    current_command: &str,
) -> Option<&'a Manifest> {
    manifests
        .values()
        .find(|m| m.agent.process_names.iter().any(|p| p == current_command))
}

/// Bind a manifest to a pane row: the explicit pin first, then the comm
/// name, then the argv-basename fallback.
///
/// The pin is `cyclops name --manifest <id>` and it wins outright. It
/// exists because both automatic routes read what the pane is RUNNING, and
/// a wrapper script, a `sh -c`, or a versioned symlink (F21) can leave a
/// real agent looking like nothing in particular. A person who says which
/// CLI is in the pane is better evidence than a process name.
///
/// pane_current_command is the kernel comm of the RESOLVED executable, so
/// native installs can report a bare version string ("2.1.220", F21) and
/// never bind by comm; the invoked argv[0] basename still says "claude".
/// The fallback resolves argv once per (pane, pid), cached, and matches it
/// against process_names plus argv_basenames.
///
/// The pid it reads is the pane's FOREGROUND process, not `pane_pid`. See
/// [`foreground_pid`]: an agent started by typing its name at a shell
/// prompt is a child of the pane's first process, and reading `pane_pid`
/// binds every such pane to the shell instead of the agent.
pub(crate) fn bind_manifest_for<'a>(inner: &'a Inner, row: &PaneRow) -> Option<&'a Manifest> {
    if let Some(pinned) = inner
        .registry
        .lock()
        .expect("registry lock")
        .manifest_of(&row.pane_id)
    {
        // A pin that names nothing loaded falls through to detection
        // rather than blinding the pane: the manifest set can shrink
        // between the adoption and this recompute.
        if let Some(m) = inner.manifests.get(&pinned) {
            return Some(m);
        }
    }
    if let Some(m) = bind_manifest(&inner.manifests, &row.current_command) {
        return Some(m);
    }
    if row.pane_pid <= 0 {
        return None;
    }
    argv_bound_manifest(inner, &row.pane_id, foreground_pid(row.pane_pid))
}

/// The manifest that claims this argv[0] basename, by either declared name.
pub(crate) fn manifest_for_basename<'a>(
    manifests: &'a BTreeMap<String, Manifest>,
    base: &str,
) -> Option<&'a Manifest> {
    manifests.values().find(|m| {
        m.agent.argv_basenames.iter().any(|name| name == base)
            || m.agent.process_names.iter().any(|name| name == base)
    })
}

/// The pid whose argv says what a pane is RUNNING.
///
/// tmux's `pane_pid` is the pane's FIRST process, which for an interactive
/// pane is the shell and stays the shell for the pane's whole life. An
/// agent the user starts by typing its name at that prompt is a child of
/// it, so `pane_pid` names the shell no matter what is on screen.
/// MEASURED (tmux 3.7b, Claude Code 2.1.222): a pane running Claude Code
/// reports `pane_current_command` "2.1.222", `pane_pid` the zsh, and
/// `ps -o args=` on that pid "-zsh" — nothing in either sensor says
/// "claude", so the pane bound no manifest and carried no state at all.
///
/// The tty's foreground process group is the job the terminal is actually
/// talking to, and a process group's id is its leader's pid, so `tpgid`
/// resolves straight to the running agent. A shell idle at its prompt is
/// its own foreground group and resolves back to `pane_pid` unchanged,
/// which is what makes the agent's exit unbind the manifest again.
fn foreground_pid(pane_pid: i32) -> i32 {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "tpgid=", "-p", &pane_pid.to_string()])
        .output()
    else {
        return pane_pid;
    };
    if !out.status.success() {
        return pane_pid;
    }
    parse_tpgid(&String::from_utf8_lossy(&out.stdout)).unwrap_or(pane_pid)
}

/// A `ps -o tpgid=` line as a pid. A pane with no controlling terminal
/// reports -1, which names no process and must not be looked up.
pub(crate) fn parse_tpgid(line: &str) -> Option<i32> {
    let value: i32 = line.trim().parse().ok()?;
    (value > 0).then_some(value)
}

/// Bind a manifest by the argv[0] basename of a pane's foreground process,
/// memoising the reading only once it has actually bound something. The ps
/// spawn runs when comm binding already missed; never on a clock.
///
/// The asymmetry — remember a hit, re-probe a miss — is load-bearing, not
/// an optimisation. Vendor CLIs ship a shell wrapper that re-execs itself
/// in place, so the pid is stable across the exec while argv[0] flips from
/// the wrapper's interpreter to the agent's own name. cursor-agent's
/// wrapper ends in `exec -a "$0" "$NODE_BIN" ... index.js "$@"`, and
/// MEASURED (cursor-agent 2026.07.23-e383d2b) pid 37750 read:
///
/// ```text
/// t+0.00s  ps args = bash /Users/x/.local/bin/agent    -> "bash",  binds nothing
/// t+0.25s  ps args = /Users/x/.local/bin/agent ...     -> "agent", binds cursor
/// ```
///
/// Recomputes are output-driven and typing `agent` at a prompt echoes that
/// line immediately, so the probe lands in the first window often. Keyed on
/// (pane, pid), a cache that remembered "bash" could never correct itself —
/// the pid never changes — and the pane would read unknown, carry no state
/// and refuse delivery for the rest of that process's life. So a basename
/// that binds nothing means "not settled yet", never "no agent here".
///
/// One entry per pane: the foreground pid changes with every job the shell
/// runs, and keeping the losers would grow the map for the pane's whole
/// life without any of them ever being read again.
pub(crate) fn argv_bound_manifest<'a>(
    inner: &'a Inner,
    pane_id: &str,
    pid: i32,
) -> Option<&'a Manifest> {
    if pid <= 0 {
        return None;
    }
    let key = (pane_id.to_string(), pid);
    let cached = inner
        .argv_cache
        .lock()
        .expect("argv cache lock")
        .get(&key)
        .cloned();
    if let Some(base) = cached {
        return manifest_for_basename(&inner.manifests, &base);
    }
    // Spawn outside the lock: ps is slower than every other holder of it.
    let base = argv_basename(pid)?;
    let bound = manifest_for_basename(&inner.manifests, &base)?;
    let mut cache = inner.argv_cache.lock().expect("argv cache lock");
    cache.retain(|(cached_pane, _), _| cached_pane != pane_id);
    cache.insert(key, base);
    Some(bound)
}

/// argv[0] basename of a live pid via `ps -o args=`.
fn argv_basename(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_argv_basename(&String::from_utf8_lossy(&out.stdout))
}

/// First whitespace-separated token of a ps args line, basename only.
pub(crate) fn parse_argv_basename(args_line: &str) -> Option<String> {
    let first = args_line.split_whitespace().next()?;
    let base = first.rsplit('/').next()?;
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// What a recompute should do with the stored hook reading.
#[derive(Debug, PartialEq, Eq)]
enum HookAction {
    /// Reading is live: feed it to fusion.
    Use,
    /// Reading is spent (TTL) or invalidated (repeated rules-tier
    /// contradiction): drop it from the store and fuse without it.
    Drop,
}

/// Age one hook entry against the rules-tier verdict of this recompute.
/// Disagreement only counts when the rules actually decided something;
/// agreement resets the streak.
fn hook_action(entry: &mut HookEntry, rules_state: AgentState, now_ms: u64) -> HookAction {
    if now_ms.saturating_sub(entry.reading.ts) > HOOK_READING_TTL_MS {
        return HookAction::Drop;
    }
    if rules_state == AgentState::Unknown {
        return HookAction::Use;
    }
    if rules_state == entry.reading.state {
        entry.disagreements = 0;
        return HookAction::Use;
    }
    entry.disagreements += 1;
    if entry.disagreements >= HOOK_DISAGREE_LIMIT {
        HookAction::Drop
    } else {
        HookAction::Use
    }
}

/// Highest-priority pane_title rule matching the title.
pub(crate) fn title_winner<'m>(m: &'m Manifest, title: &str) -> Option<&'m CompiledRule> {
    m.rules
        .iter()
        .find(|r| r.region == Region::PaneTitle && r.matches(title, &[title]))
}

/// Highest-priority screen-region rule matching the capture. Region
/// slicing matches `Manifest::evaluate`: bottom N non-empty lines,
/// restored to top-down order. Production goes through
/// [`screen_winner_esc`] (the recompute may carry an escaped capture);
/// this plain form serves the tests that assert single-capture behavior.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn screen_winner<'m>(m: &'m Manifest, screen: &str) -> Option<&'m CompiledRule> {
    screen_winner_esc(m, screen, None)
}

/// [`screen_winner`] with an optional SGR-escaped capture of the same grid
/// (capture-pane -e), so `line_regex_esc` rules can fire. Escaped lines are
/// judged non-empty on their CSI-stripped text, mirroring
/// `Manifest::evaluate_esc`, so both captures slice the same screen rows.
pub(crate) fn screen_winner_esc<'m>(
    m: &'m Manifest,
    screen: &str,
    screen_esc: Option<&str>,
) -> Option<&'m CompiledRule> {
    let non_empty: Vec<&str> = screen
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let non_empty_esc: Option<Vec<&str>> = screen_esc.map(|s| {
        s.lines()
            .rev()
            .filter(|l| !strip_csi(l).trim().is_empty())
            .collect()
    });
    m.rules.iter().find(|r| match r.region {
        Region::PaneTitle => false,
        Region::BottomNonEmptyLines(n) => {
            let mut sel: Vec<&str> = non_empty.iter().take(n).copied().collect();
            sel.reverse();
            let esc = non_empty_esc.as_ref().map(|ne| {
                let mut sel: Vec<&str> = ne.iter().take(n).copied().collect();
                sel.reverse();
                sel
            });
            r.matches_esc(&sel.join("\n"), &sel, esc.as_deref())
        }
    })
}

/// Capture the sensor set a manifest needs: the plain grid, plus the
/// SGR-escaped grid when any rule carries `line_regex_esc` clauses (codex
/// ghost vs typed text, F19). A failed escaped capture fails the whole
/// read: with the plain capture alone the esc rules fail closed and typed
/// human text reads as idle, which is the injection hazard they exist to
/// prevent. The caller's doubt handling covers both captures.
async fn capture_screens(
    watcher: &SessionWatcher,
    m: &Manifest,
    pane_id: &str,
) -> Result<(String, Option<String>), TmuxError> {
    let plain = watcher.client().capture_pane(pane_id).await?;
    if !m.has_escaped_rules() {
        return Ok((plain, None));
    }
    let esc = watcher.client().capture_pane_escaped(pane_id).await?;
    Ok((plain, Some(esc)))
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
///
/// `session_idx` is the caller's stable session-slot index, not re-derived
/// here from `watcher.session()`: see [`crate::emit_state`]'s doc comment
/// for the rename race that distinction closes. Every call site already
/// has one, from wherever it entered the session (an event's own
/// `session_task`, a resolved recipient, a delivery handle).
pub(crate) async fn recompute_pane(
    inner: &Arc<Inner>,
    session_idx: usize,
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
    let manifest = bind_manifest_for(inner, &row);
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
                        since: std::time::Instant::now(),
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
        let (screen, screen_esc) = if need_screen {
            match capture_screens(watcher, m, pane_id).await {
                Ok((s, esc)) => (Some(s), esc),
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
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        let s_rule = screen
            .as_deref()
            .and_then(|s| screen_winner_esc(m, s, screen_esc.as_deref()));
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
    // no tested CLI hooks its modals or quota (amendment h). Readings age
    // out on TTL or repeated rules-tier contradiction so a stale edge
    // cannot pin fused state.
    if !row.dead {
        let hook = {
            let mut map = inner.hook_readings.lock().expect("hook readings lock");
            match map.get_mut(pane_id) {
                None => None,
                Some(entry) => match hook_action(entry, detection.state, ts) {
                    HookAction::Use => Some(entry.reading.clone()),
                    HookAction::Drop => {
                        map.remove(pane_id);
                        None
                    }
                },
            }
        };
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
        // `since` marks the state CHANGING, so a recompute that confirms
        // the same state carries the old mark forward. Without this the
        // elapsed column would reset on every unrelated event.
        let since = match map.get(pane_id) {
            Some(e) if e.detection.state == detection.state => e.since,
            _ => std::time::Instant::now(),
        };
        map.insert(
            pane_id.to_string(),
            DetEntry {
                detection: detection.clone(),
                manifest: manifest_id,
                since,
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
        inner.emit_state(session_idx, pane_id, &detection, prior, cause);
        // The border says what this row says, from the same edge. No
        // timer, no second rule: an adopted pane's chrome moves exactly
        // when the fused state it names moves.
        crate::repaint_chrome(inner, watcher, pane_id).await;
    }
    Some(detection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StdMutex;
    use std::collections::HashMap;
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

    fn entry(state: AgentState, ts: u64) -> HookEntry {
        HookEntry::new(SensorReading {
            sensor: Sensor::Hook,
            state,
            rule: "Stop".into(),
            ts,
        })
    }

    #[test]
    fn hook_reading_ages_out_on_ttl() {
        let mut e = entry(AgentState::Working, 1_000);
        assert_eq!(
            hook_action(&mut e, AgentState::Unknown, 1_000 + HOOK_READING_TTL_MS),
            HookAction::Use
        );
        assert_eq!(
            hook_action(&mut e, AgentState::Unknown, 1_001 + HOOK_READING_TTL_MS),
            HookAction::Drop
        );
    }

    #[test]
    fn hook_reading_invalidated_by_repeated_disagreement() {
        let mut e = entry(AgentState::Working, 1_000);
        // Rules see nothing: no evidence against the hook.
        for _ in 0..10 {
            assert_eq!(
                hook_action(&mut e, AgentState::Unknown, 2_000),
                HookAction::Use
            );
        }
        // Two contradictions survive, the third invalidates.
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Use
        );
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Use
        );
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Drop
        );
    }

    #[test]
    fn hook_agreement_resets_the_disagreement_streak() {
        let mut e = entry(AgentState::Working, 1_000);
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Use
        );
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Use
        );
        assert_eq!(
            hook_action(&mut e, AgentState::Working, 2_000),
            HookAction::Use
        );
        assert_eq!(e.disagreements, 0);
        assert_eq!(
            hook_action(&mut e, AgentState::Idle, 2_000),
            HookAction::Use
        );
    }

    // The shipped codex esc rules: dim after the glyph is a ghost
    // suggestion (idle), bare text is typed input (idle_with_input), the
    // plain rule is the idle-biased fallback.
    const ESC_FIXTURE: &str = r#"
[agent]
id = "codex"
display_name = "Codex esc fixture"
process_names = ["codex"]

[[rule]]
id = "composer_typed_input"
state = "idle_with_input"
priority = 1050
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+[^\x1b\s]']

[[rule]]
id = "composer_ghost_suggestion"
state = "idle"
priority = 1040
region = "bottom_non_empty_lines(6)"
line_regex_esc = ['^\s*(?:\x1b\[[0-9;]*m)*›(?:\x1b\[[0-9;]*m)*\s+\x1b\[2m']

[[rule]]
id = "composer_empty_or_ghost"
state = "idle"
priority = 1000
region = "bottom_non_empty_lines(6)"
line_regex = ['^\s*›']
"#;

    #[test]
    fn screen_winner_esc_discriminates_typed_from_ghost() {
        let m = Manifest::parse(ESC_FIXTURE, Path::new("codex.toml")).unwrap();
        let typed_plain = "› fix the rate limiter in gateway.rs";
        let typed_esc = "\u{1b}[1m›\u{1b}[0m fix the rate limiter in gateway.rs";
        let ghost_plain = "› Find and fix a bug in @filename";
        let ghost_esc = "\u{1b}[1m›\u{1b}[0m \u{1b}[2mFind and fix a bug in @filename\u{1b}[0m";

        // With the escaped capture the esc rules decide.
        let r = screen_winner_esc(&m, typed_plain, Some(typed_esc)).unwrap();
        assert_eq!(r.id, "composer_typed_input");
        assert_eq!(r.state, AgentState::IdleWithInput);
        let r = screen_winner_esc(&m, ghost_plain, Some(ghost_esc)).unwrap();
        assert_eq!(r.id, "composer_ghost_suggestion");
        assert_eq!(r.state, AgentState::Idle);

        // Without one the esc rules fail closed: idle-biased fallback,
        // which is exactly the gap the daemon-side capture closes.
        let r = screen_winner(&m, typed_plain).unwrap();
        assert_eq!(r.id, "composer_empty_or_ghost");
        assert_eq!(r.state, AgentState::Idle);

        assert!(m.has_escaped_rules());
        assert!(!manifest().has_escaped_rules());
    }

    #[test]
    fn argv_basename_parses_ps_args_output() {
        assert_eq!(
            parse_argv_basename("/Users/x/.local/bin/claude --continue\n"),
            Some("claude".into())
        );
        assert_eq!(parse_argv_basename("  cat  \n"), Some("cat".into()));
        assert_eq!(parse_argv_basename("\n"), None);
        assert_eq!(parse_argv_basename(""), None);
    }

    const SLEEP_FIXTURE: &str = r#"
[agent]
id = "sleeper"
display_name = "Sleep fixture"
process_names = []
argv_basenames = ["sleep"]

[[rule]]
id = "title_idle"
state = "idle"
priority = 1000
region = "pane_title"
regex = ['^IDLE']
"#;

    fn inner_with(manifests: BTreeMap<String, Manifest>) -> Arc<Inner> {
        let home = cyclops_proto::scratch::scratch_dir("cyc-argv-cache");
        let (registry, _) = crate::registry::Registry::load(&home);
        Arc::new(Inner {
            cfg: crate::Config::defaults(&home),
            boot_id: "b-test".into(),
            started: std::time::Instant::now(),
            tmux_version: "3.6a".into(),
            manifests,
            manifest_dir: None,
            sessions: StdMutex::new(Vec::new()),
            events: tokio::sync::broadcast::channel(16).0,
            detections: StdMutex::new(HashMap::new()),
            registry: StdMutex::new(registry),
            theme: StdMutex::new(cyclops_theme::ThemeWatch::new(&home)),
            hook_readings: StdMutex::new(HashMap::new()),
            argv_cache: StdMutex::new(HashMap::new()),
            engine: crate::delivery::Engine::new(),
            ack_state: crate::ack::AckState::new(),
            hook_liveness: crate::selftest::HookLiveness::new(),
            inject_pause: StdMutex::new(None),
            fail_chrome_restore: std::sync::atomic::AtomicBool::new(false),
            workspace_ui: StdMutex::new(crate::workspace_ui::WorkspaceUiState::default()),
            stop: tokio::sync::watch::channel(false).1,
            extra_tasks: StdMutex::new(Vec::new()),
        })
    }

    /// A wrapper caught before its `exec` reads as the interpreter and binds
    /// nothing. Remembering that would pin the pane unknown for the life of
    /// the process, because the exec keeps the pid the cache is keyed on.
    #[test]
    fn a_basename_that_binds_nothing_is_re_probed_not_memoised() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;

        // No manifest claims "sleep": the reading is a miss, and a miss must
        // leave the cache empty so the next recompute probes again.
        let blind = inner_with(BTreeMap::new());
        assert!(argv_bound_manifest(&blind, "%0", pid).is_none());
        assert!(
            blind.argv_cache.lock().unwrap().is_empty(),
            "a non-binding basename must not be memoised"
        );

        // The same pid, once a manifest claims it, binds and is remembered.
        let mut map = BTreeMap::new();
        map.insert(
            "sleeper".to_string(),
            Manifest::parse(SLEEP_FIXTURE, Path::new("sleeper.toml")).unwrap(),
        );
        let bound = inner_with(map);
        assert_eq!(
            argv_bound_manifest(&bound, "%0", pid).map(|m| m.agent.id.as_str()),
            Some("sleeper")
        );
        assert_eq!(
            bound
                .argv_cache
                .lock()
                .unwrap()
                .get(&("%0".to_string(), pid)),
            Some(&"sleep".to_string()),
            "a binding basename is memoised for the pane"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn basename_binding_matches_either_declared_name() {
        let mut map = BTreeMap::new();
        map.insert("bash".to_string(), manifest());
        // process_names
        assert_eq!(
            manifest_for_basename(&map, "bash").map(|m| m.agent.id.as_str()),
            Some("bash")
        );
        // the wrapper's pre-exec interpreter is not a claim on the pane
        assert!(manifest_for_basename(&map, "node").is_none());
        assert!(manifest_for_basename(&BTreeMap::new(), "bash").is_none());
    }

    /// MEASURED 2026-08-06 (Claude Code 2.1.221, tmux 3.6a, live rig): a
    /// pane running a native claude read pane_current_command "2.1.221"
    /// (version symlink, F21), `ps -o args=` on pane_pid "-zsh", and
    /// tpgid " 19989\n" whose args were "claude". Pins every hop of the
    /// binding chain against the shipped manifests on that data alone.
    #[test]
    fn measured_claude_binding_triple_2_1_221() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
        let shipped: BTreeMap<String, Manifest> = cyclops_manifest::load_dir(&dir)
            .unwrap()
            .into_iter()
            .collect();

        // Comm route: nothing claims the bare version string.
        assert!(bind_manifest(&shipped, "2.1.221").is_none());

        // pane_pid's argv is the login shell and binds nothing.
        let shell = parse_argv_basename("-zsh\n").unwrap();
        assert_eq!(shell, "-zsh");
        assert!(manifest_for_basename(&shipped, &shell).is_none());

        // The measured tpgid line resolves to the foreground group leader.
        assert_eq!(parse_tpgid(" 19989\n"), Some(19989));

        // That leader's argv is what binds the claude manifest.
        let agent = parse_argv_basename("claude\n").unwrap();
        assert_eq!(
            manifest_for_basename(&shipped, &agent).map(|m| m.agent.id.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn tpgid_parses_ps_output_and_rejects_no_terminal() {
        assert_eq!(parse_tpgid("  6254\n"), Some(6254));
        // A pane with no controlling terminal: -1 names no process, and 0
        // is not a pid either. Both must fall back to pane_pid rather than
        // send `ps -p` after something that cannot exist.
        assert_eq!(parse_tpgid("   -1\n"), None);
        assert_eq!(parse_tpgid("0\n"), None);
        assert_eq!(parse_tpgid("\n"), None);
        assert_eq!(parse_tpgid("not a pid\n"), None);
    }
}
