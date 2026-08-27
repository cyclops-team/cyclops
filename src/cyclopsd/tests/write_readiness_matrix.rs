//! Deterministic manifest and protocol fixture matrix.
//!
//! Evaluates manifest rule evaluation and protocol Detection fusion invariants
//! across all 9 agent states for Claude Code, Codex CLI, and Antigravity CLI,
//! with a separate non-authoritative fixture test for Cursor.
//!
//! This is a deterministic protocol and manifest fixture test, not a live
//! runtime gate: it verifies that only a genuinely clean idle composer with
//! matching screen evidence can produce `write_ready == true`, while non-idle
//! states, drafts, active turns, modals, permission prompts, quota exhaustion,
//! unknown titles, dead processes, and sensor disagreements reliably hold or refuse.

use std::path::Path;

use cyclops_manifest::{load_dir, Manifest};
use cyclops_proto::{AgentState, ComposerHold, Detection, Sensor, SensorReading};

fn shipped_manifests() -> std::collections::HashMap<String, Manifest> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/manifests");
    load_dir(&dir).expect("load shipped manifests")
}

/// Construct a Detection from a shipped screen rule and optional hook reading.
fn fuse_detection(
    vendor: &str,
    state: AgentState,
    decided_by: &str,
    hook_reading: Option<AgentState>,
    stale: bool,
    disagreement: bool,
) -> Detection {
    let composer_semantic = shipped_manifests()
        .get(vendor)
        .and_then(|manifest| manifest.rules.iter().find(|rule| rule.id == decided_by))
        .and_then(|rule| rule.composer_semantic);
    let mut readings = Vec::new();
    readings.push(SensorReading {
        sensor: Sensor::Screen,
        state,
        ts: 1000,
        rule: format!("manifest:{vendor}"),
    });
    if let Some(h_state) = hook_reading {
        readings.push(SensorReading {
            sensor: Sensor::Hook,
            state: h_state,
            ts: 1000,
            rule: format!("hook:{vendor}"),
        });
    }

    let det = Detection {
        state,
        readings,
        disagreement,
        decided_by: decided_by.to_string(),
        unknown_reason: None,
        stale,
        write_ready: false,
        write_block: None,
        composer_semantic,
    };
    det.stamped(false, ComposerHold::Clear)
}

// Claude Code fixtures.

#[test]
fn claude_deterministic_safety_matrix_9_states() {
    let manifests = shipped_manifests();
    let manifest = &manifests["claude"];

    // 1. Idle (clean composer) -> write_ready == true
    let clean_screen = "Done\n────────────────────────────────────────\n❯ \n────────────────────────────────────────\n  Haiku 4.5 · /tmp · Ctx: 86%";
    let clean_esc = clean_screen.replace("❯ \n", "\u{1b}[39m❯\u{a0}\n");
    let eval = manifest
        .evaluate_esc("Done", clean_screen, Some(&clean_esc))
        .unwrap();
    assert_eq!(eval.state, AgentState::Idle);
    let det = fuse_detection(
        "claude",
        AgentState::Idle,
        &eval.id,
        Some(AgentState::Idle),
        false,
        false,
    );
    assert!(
        det.write_ready,
        "clean idle claude must be write_ready: {det:?}"
    );

    // 2. IdleWithInput (human typed draft) -> write_ready == false
    let typed_screen = clean_screen.replace("❯ \n", "❯ draft text\n");
    let typed_esc = clean_esc.replace("\u{1b}[39m❯\u{a0}\n", "\u{1b}[39m❯\u{a0}draft text\n");
    let eval = manifest
        .evaluate_esc("Done", &typed_screen, Some(&typed_esc))
        .unwrap();
    assert_eq!(eval.state, AgentState::IdleWithInput);
    let det = fuse_detection(
        "claude",
        AgentState::IdleWithInput,
        &eval.id,
        Some(AgentState::Idle),
        false,
        false,
    );
    assert!(!det.write_ready, "staged draft must refuse write");
    assert_eq!(det.write_block.as_deref(), Some("not_idle"));

    // 3. Working (generation turn active via spinner status) -> write_ready == false
    let working_screen = "Thinking\n────────────────────────────────────────\n· Kneading… (5s · 1.2k tokens)\n❯ \n────────────────────────────────────────";
    let working_esc = "Thinking\n────────────────────────────────────────\n\u{1b}[38;5;215m·\u{1b}[39m \u{1b}[38;5;215mKneading… \u{1b}[38;5;246m(5s · 1.2k tokens)\n\u{1b}[39m❯\u{a0}\n────────────────────────────────────────";
    let eval = manifest
        .evaluate_esc("Thinking", working_screen, Some(working_esc))
        .unwrap();
    assert_eq!(eval.state, AgentState::Working);
    let det = fuse_detection(
        "claude",
        AgentState::Working,
        &eval.id,
        Some(AgentState::Working),
        false,
        false,
    );
    assert!(!det.write_ready, "working agent must refuse write");

    // 4. BlockedModal (trust dialog) -> write_ready == false
    let trust_dialog = "Quick safety check: Is this a project you created or one you trust?\n1. Yes, I trust this folder\n2. No, exit\nEnter to confirm";
    let eval = manifest.evaluate("Trust", trust_dialog).unwrap();
    assert_eq!(eval.state, AgentState::BlockedModal);
    let det = fuse_detection(
        "claude",
        AgentState::BlockedModal,
        &eval.id,
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "trust modal must refuse write");

    // 5. BlockedPermission (permission prompt) -> write_ready == false
    let perm_prompt =
        "Claude needs permission to edit file.rs\nDo you want to allow this?\n1. Yes\n2. No";
    let eval = manifest.evaluate("Permission", perm_prompt).unwrap();
    assert_eq!(eval.state, AgentState::BlockedPermission);
    let det = fuse_detection(
        "claude",
        AgentState::BlockedPermission,
        &eval.id,
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "permission prompt must refuse write");

    // 6. BlockedQuota (rate limit / quota exhausted) -> write_ready == false
    let det = fuse_detection(
        "claude",
        AgentState::BlockedQuota,
        "quota_blocked",
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "quota block must refuse write");

    // 7. Unknown (unrecognized screen) -> write_ready == false
    let det = fuse_detection("claude", AgentState::Unknown, "unknown", None, false, false);
    assert!(!det.write_ready, "unknown state must refuse write");

    // 8. Dead (process exited) -> write_ready == false
    let det = fuse_detection(
        "claude",
        AgentState::Dead,
        "process_dead",
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "dead process must refuse write");

    // 9. Sensor Disagreement (hook says idle, screen says working) -> write_ready == false
    let det = fuse_detection(
        "claude",
        AgentState::Idle,
        "hook_idle",
        Some(AgentState::Idle),
        false,
        true,
    );
    assert!(!det.write_ready, "sensor disagreement must refuse write");
    assert_eq!(det.write_block.as_deref(), Some("sensor_disagreement"));
}

// Codex CLI fixtures.

#[test]
fn codex_deterministic_safety_matrix_9_states() {
    let manifests = shipped_manifests();
    let manifest = &manifests["codex"];

    // 1. Idle (clean composer) -> write_ready == true
    let clean_screen = "› \ngpt-5.6-sol high · ~/proj";
    let clean_esc = "\u{1b}[1m›\u{1b}[0m \u{1b}[2mAsk Codex to do anything\u{1b}[0m\n\u{1b}[38;2;246;226;183mgpt-5.6-sol high\u{1b}[0m · ~/proj";
    let eval = manifest
        .evaluate_esc("proj", clean_screen, Some(clean_esc))
        .unwrap();
    assert_eq!(eval.state, AgentState::Idle);
    let det = fuse_detection(
        "codex",
        AgentState::Idle,
        &eval.id,
        Some(AgentState::Idle),
        false,
        false,
    );
    assert!(det.write_ready, "clean idle codex must be write_ready");

    // 2. IdleWithInput (human typed draft) -> write_ready == false
    let typed_screen = "› my typed draft text\ngpt-5.6-sol high · ~/proj";
    let typed_esc = "\u{1b}[1m›\u{1b}[0m my typed draft text\n\u{1b}[38;2;246;226;183mgpt-5.6-sol high\u{1b}[0m · ~/proj";
    let eval = manifest
        .evaluate_esc("proj", typed_screen, Some(typed_esc))
        .unwrap();
    assert_eq!(eval.state, AgentState::IdleWithInput);
    let det = fuse_detection(
        "codex",
        AgentState::IdleWithInput,
        &eval.id,
        Some(AgentState::Idle),
        false,
        false,
    );
    assert!(!det.write_ready, "staged draft must refuse write");

    // 3. Working (generation turn active) -> write_ready == false
    let working_screen = "• Working (5s • esc to interrupt)\n› Ask Codex\ngpt-5.6-sol";
    let eval = manifest.evaluate("Working", working_screen).unwrap();
    assert_eq!(eval.state, AgentState::Working);
    let det = fuse_detection(
        "codex",
        AgentState::Working,
        &eval.id,
        Some(AgentState::Working),
        false,
        false,
    );
    assert!(!det.write_ready, "working turn must refuse write");

    // 4. BlockedModal (approval / confirmation modal) -> write_ready == false
    let det = fuse_detection(
        "codex",
        AgentState::BlockedModal,
        "command_confirmation",
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "modal prompt must refuse write");

    // 5. BlockedPermission -> write_ready == false
    let det = fuse_detection(
        "codex",
        AgentState::BlockedPermission,
        "permission_required",
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "permission required must refuse write");

    // 6. BlockedQuota (usage reset / rate limit) -> write_ready == false
    let det = fuse_detection(
        "codex",
        AgentState::BlockedQuota,
        "quota_limit",
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "quota limit must refuse write");

    // 7. Unknown -> write_ready == false
    let det = fuse_detection("codex", AgentState::Unknown, "unknown", None, false, false);
    assert!(!det.write_ready, "unknown state must refuse write");

    // 8. Dead -> write_ready == false
    let det = fuse_detection("codex", AgentState::Dead, "dead", None, false, false);
    assert!(!det.write_ready, "dead process must refuse write");

    // 9. Stale Screen Evidence -> write_ready == false
    let det = fuse_detection(
        "codex",
        AgentState::Idle,
        &eval.id,
        Some(AgentState::Idle),
        true,
        false,
    );
    assert!(!det.write_ready, "stale screen evidence must refuse write");
    assert_eq!(det.write_block.as_deref(), Some("stale_screen_evidence"));
}

// Antigravity CLI fixtures.

#[test]
fn agy_deterministic_safety_matrix_9_states() {
    let manifests = shipped_manifests();
    let manifest = &manifests["agy"];

    // 1. Idle (clean composer with bare >) -> write_ready == true
    let clean_screen =
        "mac\n────────────────\n>\n────────────────\nGemini 3.6 Flash (High) · High · ~ · Ctx: 80%";
    let eval = manifest.evaluate("mac", clean_screen).unwrap();
    assert_eq!(eval.state, AgentState::Idle);
    let det = fuse_detection(
        "agy",
        AgentState::Idle,
        &eval.id,
        Some(AgentState::Idle),
        false,
        false,
    );
    assert!(
        det.write_ready,
        "clean idle agy must be write_ready: {det:?}"
    );

    // 2. IdleWithInput (human typed draft) -> write_ready == false
    let typed_screen =
        "mac\n────────────────\n> my draft text\n────────────────\nGemini 3.6 Flash (High)";
    let typed_esc = "mac\n\u{1b}[90m────────────────\n\u{1b}[94m>\u{1b}[39m my draft text\n\u{1b}[90m────────────────\n\u{1b}[38;5;152mGemini 3.6 Flash (High)";
    let eval = manifest
        .evaluate_esc("mac", typed_screen, Some(typed_esc))
        .unwrap();
    assert_eq!(eval.state, AgentState::IdleWithInput);
    let det = fuse_detection(
        "agy",
        AgentState::IdleWithInput,
        &eval.id,
        Some(AgentState::Idle),
        false,
        false,
    );
    assert!(!det.write_ready, "staged draft must refuse write");
    assert_eq!(det.write_block.as_deref(), Some("not_idle"));

    // 3. Working (generation turn active via braille spinner) -> write_ready == false
    let working_screen = "mac\n────────────────\n⣷ Generating...\n>\n────────────────";
    let eval = manifest.evaluate("mac", working_screen).unwrap();
    assert_eq!(eval.state, AgentState::Working);
    let det = fuse_detection(
        "agy",
        AgentState::Working,
        &eval.id,
        Some(AgentState::Working),
        false,
        false,
    );
    assert!(!det.write_ready, "working turn must refuse write");

    // 4. BlockedModal (feedback survey modal) -> write_ready == false
    let survey_screen = "How's the CLI experience so far?\n[1] Good  [2] Fine  [3] Bad  [0] Skip";
    let eval = manifest.evaluate("mac", survey_screen).unwrap();
    assert_eq!(eval.state, AgentState::BlockedModal);
    let det = fuse_detection(
        "agy",
        AgentState::BlockedModal,
        &eval.id,
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "survey modal must refuse write");

    // 5. BlockedPermission -> write_ready == false
    let det = fuse_detection(
        "agy",
        AgentState::BlockedPermission,
        "permission_gate",
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "permission prompt must refuse write");

    // 6. BlockedQuota (quota exhausted) -> write_ready == false
    let quota_screen =
        "Individual quota reached. Please upgrade your subscription to increase your limits.";
    let eval = manifest.evaluate("mac", quota_screen).unwrap();
    assert_eq!(eval.state, AgentState::BlockedQuota);
    let det = fuse_detection(
        "agy",
        AgentState::BlockedQuota,
        &eval.id,
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "quota exhaustion must refuse write");

    // 7. Unknown (title_useless rule priority 0) -> write_ready == false
    let det = fuse_detection(
        "agy",
        AgentState::Unknown,
        "title_useless",
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "unknown state must refuse write");

    // 8. Dead -> write_ready == false
    let det = fuse_detection("agy", AgentState::Dead, "dead", None, false, false);
    assert!(!det.write_ready, "dead process must refuse write");

    // 9. Sensor Disagreement (hook says idle, screen says working) -> write_ready == false
    let det = fuse_detection(
        "agy",
        AgentState::Idle,
        "hook_idle",
        Some(AgentState::Idle),
        false,
        true,
    );
    assert!(!det.write_ready, "sensor disagreement must refuse write");
    assert_eq!(det.write_block.as_deref(), Some("sensor_disagreement"));
}

// ── Separate Non-Authoritative Fixture: Cursor ────────────────────────────────

#[test]
fn cursor_offline_fixture_validation_unavailable_live_gate() {
    let manifests = shipped_manifests();
    let manifest = &manifests["cursor"];

    // 1. Idle (clean composer with dim placeholder) -> write_ready == true
    let clean_screen =
        "Composer\n────────────────\n→ Plan, search, build\n────────────────\nCursor 0.45";
    let clean_esc = "Composer\n────────────────\n\u{1b}[2m→\u{1b}[0m \u{1b}[2mPlan, search, build\u{1b}[0m\n────────────────\nCursor 0.45";
    let eval = manifest
        .evaluate_esc("Cursor Agent", clean_screen, Some(clean_esc))
        .unwrap();
    assert_eq!(eval.state, AgentState::Idle);
    let det = fuse_detection(
        "cursor",
        AgentState::Idle,
        &eval.id,
        Some(AgentState::Idle),
        false,
        false,
    );
    assert!(
        !det.write_ready,
        "the offline Cursor fixture cannot prove a write-safe composer"
    );
    assert_eq!(
        det.write_block.as_deref(),
        Some("no_write_safe_composer_evidence")
    );

    // 2. IdleWithInput (human typed draft) -> write_ready == false
    let typed_screen =
        "Composer\n────────────────\n→ refactor the parser\n────────────────\nCursor 0.45";
    let typed_esc =
        "Composer\n────────────────\n→ refactor the parser\n────────────────\nCursor 0.45";
    let eval = manifest
        .evaluate_esc("Cursor Agent", typed_screen, Some(typed_esc))
        .unwrap();
    assert_eq!(eval.state, AgentState::IdleWithInput);
    let det = fuse_detection(
        "cursor",
        AgentState::IdleWithInput,
        &eval.id,
        Some(AgentState::Idle),
        false,
        false,
    );
    assert!(!det.write_ready, "staged draft must refuse write");

    // 3. Working (agent actively generating) -> write_ready == false
    let working_screen = "Generating response...\nctrl+c to stop\n────────────────\nCursor 0.45";
    let eval = manifest.evaluate("Cursor Agent", working_screen).unwrap();
    assert_eq!(eval.state, AgentState::Working);
    let det = fuse_detection(
        "cursor",
        AgentState::Working,
        &eval.id,
        Some(AgentState::Working),
        false,
        false,
    );
    assert!(!det.write_ready, "working cursor must refuse write");

    // 4. BlockedModal (trust dialog with active selection marker) -> write_ready == false
    let trust_dialog =
        "Do you trust the contents of this directory?\n▶ [a] Trust this workspace\n  [q] Quit";
    let eval = manifest.evaluate("Cursor Agent", trust_dialog).unwrap();
    assert_eq!(eval.state, AgentState::BlockedModal);
    let det = fuse_detection(
        "cursor",
        AgentState::BlockedModal,
        &eval.id,
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "modal must refuse write");

    // 5. BlockedPermission (approval prompt) -> write_ready == false
    let approval_prompt = "Waiting for approval...\nRun this command?\n  → Run (once) (y)";
    let eval = manifest.evaluate("Cursor Agent", approval_prompt).unwrap();
    assert_eq!(eval.state, AgentState::BlockedPermission);
    let det = fuse_detection(
        "cursor",
        AgentState::BlockedPermission,
        &eval.id,
        None,
        false,
        false,
    );
    assert!(!det.write_ready, "permission prompt must refuse write");
}
