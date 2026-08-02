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
