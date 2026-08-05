//! Data-safety tests for the shipped manifests, locked against real
//! captures from the M1 soak and the codex ghost probe (2026-08-02).
//! These encode the M1 gate fixes; loosening them reopens measured
//! injection hazards.

use std::path::Path;

use cyclops_manifest::{load_dir, Manifest};
use cyclops_proto::AgentState;

fn shipped() -> std::collections::HashMap<String, Manifest> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../manifests");
    load_dir(&dir).unwrap()
}

/// M1 review, HIGH: with the sparkle title at 1000 and staged input at 950,
/// the fused state read idle while a human draft sat in the composer, and
/// the gate would paste over it and auto-submit. The staged-input rule must
/// outrank the idle title.
#[test]
fn claude_staged_input_outranks_idle_sparkle() {
    let all = shipped();
    let claude = &all["claude"];
    // Idle-shaped screen except for a human draft on the composer line.
    let screen = "\u{273b} Crunched for 1s\n\
        ────────────────────────────────────────\n\
        ❯ draft text\n\
        ────────────────────────────────────────\n\
        \u{20}\u{20}Haiku 4.5 · /tmp/proj · Ctx: 86%\n\
        \u{20}\u{20}⏵⏵ bypass permissions on (shift+tab to cycle)";
    let r = claude.evaluate("\u{2733} Done", screen).unwrap();
    assert_eq!(r.id, "composer_has_staged_input");
    assert_eq!(r.state, AgentState::IdleWithInput);

    // The same screen with an empty composer still reads idle by title.
    let idle_screen = screen.replace("❯ draft text", "❯ ");
    let r = claude.evaluate("\u{2733} Done", &idle_screen).unwrap();
    assert_eq!(r.state, AgentState::Idle);
}

/// M1 soak, SAFETY: Claude 2.1.220's folder-trust dialog contains 'Enter to
/// confirm', so startup_modal (auto_dismiss, decline Escape) matched it, and
/// Escape EXITS the CLI. The dedicated trust_dialog rule must win and must
/// never auto-dismiss: trust is a human decision.
#[test]
fn claude_trust_dialog_never_auto_dismissed() {
    let all = shipped();
    let claude = &all["claude"];
    let dialog = include_str!("fixtures/claude_trust_dialog.txt");

    let r = claude.evaluate("plain title", dialog).unwrap();
    assert_eq!(r.id, "trust_dialog");
    assert_eq!(r.state, AgentState::BlockedModal);
    assert!(!r.auto_dismiss, "trust dialog must park, not auto-dismiss");
    assert!(r.decline_keys.is_empty(), "no keys: Escape exits the CLI");

    // The shadow hazard is real: startup_modal alone matches this capture.
    // Only the priority ordering keeps it away from the decline path.
    let startup = claude
        .rules
        .iter()
        .find(|r| r.id == "startup_modal")
        .expect("startup_modal rule");
    let lines: Vec<&str> = dialog.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(startup.matches(dialog, &lines));
    let trust = claude
        .rules
        .iter()
        .find(|r| r.id == "trust_dialog")
        .expect("trust_dialog rule");
    assert!(trust.priority > startup.priority);
}

/// M1 review, HIGH: codex ghost suggestions and typed text are identical in
/// the plain capture, so idle_with_input was unreachable. MEASURED (ghost
/// probe, codex-cli 0.146.0): SGR discriminates: ghost text is dim
/// (ESC[2m), typed text is bare. Locked against the probe's real captures.
#[test]
fn codex_ghost_vs_typed_probed_fixtures() {
    let all = shipped();
    let codex = &all["codex"];
    let ghost_plain = include_str!("fixtures/codex_ghost_composer_plain.txt");
    let ghost_esc = include_str!("fixtures/codex_ghost_composer_esc.txt");
    let typed_plain = include_str!("fixtures/codex_typed_composer_plain.txt");
    let typed_esc = include_str!("fixtures/codex_typed_composer_esc.txt");

    // Pristine composer, ghost suggestion rendered dim: idle, safe.
    let r = codex
        .evaluate_esc("proj", ghost_plain, Some(ghost_esc))
        .unwrap();
    assert_eq!(r.id, "composer_ghost_suggestion");
    assert_eq!(r.state, AgentState::Idle);

    // Typed literal text: idle_with_input, never inject over it.
    let r = codex
        .evaluate_esc("proj", typed_plain, Some(typed_esc))
        .unwrap();
    assert_eq!(r.id, "composer_typed_input");
    assert_eq!(r.state, AgentState::IdleWithInput);

    // Without an escaped capture the esc rules fail closed and the plain
    // fallback still calls typed text idle. This documents the residual
    // gap until the daemon supplies capture-pane -e captures.
    let r = codex.evaluate("proj", typed_plain).unwrap();
    assert_eq!(r.id, "composer_empty_or_ghost");
    assert_eq!(r.state, AgentState::Idle);
}

/// M1 soak, BINDING: native Claude installs report pane_current_command as
/// the version string ("2.1.220"), so process_names never binds. The
/// manifest must carry the argv fallback data the daemon binds with.
#[test]
fn claude_carries_argv_basenames_fallback() {
    let all = shipped();
    let claude = &all["claude"];
    assert_eq!(claude.agent.process_names, vec!["claude"]);
    assert_eq!(claude.agent.argv_basenames, vec!["claude"]);
}

/// Every non-empty line of a capture, the shape `matches` wants for showing
/// that a rule would have fired on its own.
fn non_empty(capture: &str) -> Vec<&str> {
    capture.lines().filter(|l| !l.trim().is_empty()).collect()
}

/// MEASURED 2026-08-05, BINDING: the Cursor entry points are symlinks to a
/// bash wrapper whose last line is `exec -a "$0" "$NODE_BIN" index.js`, so
/// the surviving process is the bundled node and tmux reports
/// pane_current_command = "node". Binding on that name would claim every
/// node process on the machine, so this manifest binds on argv alone —
/// which the same exec makes reliable, since it sets argv[0] to the invoked
/// path. "node" must never appear in process_names.
#[test]
fn cursor_binds_on_argv_and_never_claims_node() {
    let all = shipped();
    let cursor = &all["cursor"];
    assert_eq!(
        cursor.agent.argv_basenames,
        vec!["agent", "cursor-agent"],
        "both installed entry-point names must bind"
    );
    assert!(
        !cursor.agent.process_names.iter().any(|n| n == "node"),
        "binding on the bundled node runtime would claim every node process"
    );
}

/// MEASURED 2026-08-05, SAFETY: Cursor keeps a dim placeholder in the
/// composer for the WHOLE turn ("Add a follow-up" while it generates), so
/// the composer rules match continuously while the agent is working. Codex
/// orders its composer rules ABOVE screen_working; copying that order here
/// would make a Cursor pane read idle for the entire turn and let the gate
/// paste into a live generation. Working must outrank the composer.
#[test]
fn cursor_working_outranks_the_live_composer_placeholder() {
    let all = shipped();
    let cursor = &all["cursor"];
    let plain = include_str!("fixtures/cursor_working_composer_plain.txt");
    let esc = include_str!("fixtures/cursor_working_composer_esc.txt");

    let r = cursor
        .evaluate_esc("Cursor Agent", plain, Some(esc))
        .unwrap();
    assert_eq!(r.id, "screen_working");
    assert_eq!(r.state, AgentState::Working);

    // The shadow is real: the idle composer rule matches this same capture,
    // because the placeholder never goes away. Only the ordering keeps the
    // pane from reading idle mid-turn.
    let ghost = cursor
        .rules
        .iter()
        .find(|r| r.id == "composer_ghost_or_empty")
        .expect("composer_ghost_or_empty rule");
    assert!(ghost.matches_esc(plain, &non_empty(plain), Some(&non_empty(esc))));
    let working = cursor
        .rules
        .iter()
        .find(|r| r.id == "screen_working")
        .expect("screen_working rule");
    assert!(working.priority > ghost.priority);
}

/// MEASURED 2026-08-05, SAFETY: answering Cursor's workspace-trust dialog
/// does NOT clear it from the screen. "Do you trust the contents of this
/// directory?" is still on the pane long after the agent is idle and taking
/// prompts, so a rule matching the question alone reports blocked_modal
/// forever and parks every delivery to a healthy pane. The '▶' selection
/// marker is present only while the dialog owns the keyboard.
#[test]
fn cursor_trust_dialog_ignores_one_already_answered() {
    let all = shipped();
    let cursor = &all["cursor"];
    let live = include_str!("fixtures/cursor_trust_dialog.txt");
    // Captured after pressing [a]: the agent is idle and usable, and the
    // dialog's text is still on screen above the composer.
    let answered = include_str!("fixtures/cursor_ghost_composer_plain.txt");

    let r = cursor.evaluate("Cursor Agent", live).unwrap();
    assert_eq!(r.id, "trust_dialog");
    assert_eq!(r.state, AgentState::BlockedModal);
    assert!(
        !r.auto_dismiss,
        "declining quits the CLI; trust is a human decision"
    );

    // The answered capture still carries the question, so the second clause
    // is what does the work here, not the region size.
    assert!(
        answered.contains("Do you trust the contents of this directory"),
        "fixture no longer exercises the stale-dialog hazard"
    );
    let r = cursor.evaluate("Cursor Agent", answered).unwrap();
    assert_eq!(
        r.state,
        AgentState::Idle,
        "an answered dialog is not a modal"
    );
}

/// MEASURED 2026-08-05: Cursor renders the approval prompt's selection
/// marker with the SAME '→' glyph the composer uses, so on a plain capture
/// the composer rules also match an approval screen. The approval rule
/// outranking them is what keeps a pane waiting on a human from reading as
/// idle_with_input and taking a delivery.
#[test]
fn cursor_approval_outranks_the_shared_arrow_glyph() {
    let all = shipped();
    let cursor = &all["cursor"];
    let prompt = include_str!("fixtures/cursor_approval_prompt.txt");

    let r = cursor.evaluate("Cursor Agent", prompt).unwrap();
    assert_eq!(r.id, "approval_prompt");
    assert_eq!(r.state, AgentState::BlockedPermission);
    assert!(!r.auto_dismiss, "an approval belongs to the human");

    let fallback = cursor
        .rules
        .iter()
        .find(|r| r.id == "composer_plain_fallback")
        .expect("composer_plain_fallback rule");
    assert!(fallback.matches(prompt, &non_empty(prompt)));
    let approval = cursor
        .rules
        .iter()
        .find(|r| r.id == "approval_prompt")
        .expect("approval_prompt rule");
    assert!(approval.priority > fallback.priority);
}

/// MEASURED 2026-08-05: an empty Cursor composer renders a placeholder whose
/// dim (ESC[2m) starts AT the glyph — the opposite arrangement from Codex,
/// where the glyph is bold and only the ghost text after it is dim. The
/// placeholder text itself changes between the first turn and later ones
/// ("Plan, search, build anything" then "Add a follow-up"), so only the dim
/// glyph is safe to match.
#[test]
fn cursor_ghost_vs_typed_probed_fixtures() {
    let all = shipped();
    let cursor = &all["cursor"];
    let ghost_plain = include_str!("fixtures/cursor_ghost_composer_plain.txt");
    let ghost_esc = include_str!("fixtures/cursor_ghost_composer_esc.txt");
    let typed_plain = include_str!("fixtures/cursor_typed_composer_plain.txt");
    let typed_esc = include_str!("fixtures/cursor_typed_composer_esc.txt");

    let r = cursor
        .evaluate_esc("Cursor Agent", ghost_plain, Some(ghost_esc))
        .unwrap();
    assert_eq!(r.id, "composer_ghost_or_empty");
    assert_eq!(r.state, AgentState::Idle);

    let r = cursor
        .evaluate_esc("Cursor Agent", typed_plain, Some(typed_esc))
        .unwrap();
    assert_eq!(r.id, "composer_typed_input");
    assert_eq!(r.state, AgentState::IdleWithInput);

    // Without an escaped capture the esc rules fail closed and the plain
    // fallback calls typed text idle. Same residual gap as codex, recorded
    // here so a future change to the daemon's capture path is noticed.
    let r = cursor.evaluate("Cursor Agent", typed_plain).unwrap();
    assert_eq!(r.id, "composer_plain_fallback");
    assert_eq!(r.state, AgentState::Idle);
}
